mod agent;
mod audio;
mod tts;

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use agent::{AgentStatus, DeepSeekAgent, McAgentOutput, TtsSegment};
use audio::VirtualMicPlayer;
use eframe::egui;
use tts::OpenAiTtsClient;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Agent,
    Speak,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Agent => "Agent (JSON)",
            Self::Speak => "Speak (CABLE Input)",
        }
    }
}

enum WorkerMessage {
    Finished(Result<String, String>),
}

struct SvmicApp {
    input: String,
    language: String,
    mode: Mode,
    output: String,
    status: String,
    running: bool,
    worker_sender: Sender<WorkerMessage>,
    worker_receiver: Receiver<WorkerMessage>,
}

impl SvmicApp {
    fn new() -> Self {
        let (worker_sender, worker_receiver) = mpsc::channel();
        Self {
            input: String::new(),
            language: String::new(),
            mode: Mode::Agent,
            output: String::new(),
            status: "Ready".to_owned(),
            running: false,
            worker_sender,
            worker_receiver,
        }
    }

    fn start_processing(&mut self) {
        let input = self.input.trim().to_owned();
        if input.is_empty() {
            self.status = "Input error".to_owned();
            self.output = "Question input cannot be empty.".to_owned();
            return;
        }

        let language = match validate_language_name(&self.language) {
            Ok(language) => language,
            Err(error) => {
                self.status = "Input error".to_owned();
                self.output = error;
                return;
            }
        };

        let mode = self.mode;
        let sender = self.worker_sender.clone();
        self.running = true;
        self.status = "Processing...".to_owned();
        self.output.clear();

        thread::spawn(move || {
            let result = match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime.block_on(process_request(&input, language.as_deref(), mode)),
                Err(error) => Err(format!("Failed to start async runtime: {error}")),
            };
            let _ = sender.send(WorkerMessage::Finished(result));
        });
    }

    fn receive_worker_messages(&mut self) {
        while let Ok(WorkerMessage::Finished(result)) = self.worker_receiver.try_recv() {
            self.running = false;
            match result {
                Ok(output) => {
                    self.status = "Finished".to_owned();
                    self.output = output;
                }
                Err(error) => {
                    self.status = "Failed".to_owned();
                    self.output = error;
                }
            }
        }
    }
}

impl eframe::App for SvmicApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.receive_worker_messages();
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));

        ui.heading("SVMIC");
        ui.label("Turn messy audience questions into clear, broadcast-ready MC narration.");
        ui.separator();

        ui.label("Question / participant input");
        egui::ScrollArea::vertical()
            .id_salt("input_scroll")
            .max_height(180.0)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.input)
                        .desired_width(f32::INFINITY)
                        .desired_rows(6)
                        .hint_text("Tulis pertanyaan peserta di sini..."),
                );
            });

        ui.horizontal(|ui| {
            ui.label("Mode:");
            egui::ComboBox::from_id_salt("mode")
                .selected_text(self.mode.label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.mode, Mode::Agent, Mode::Agent.label());
                    ui.selectable_value(&mut self.mode, Mode::Speak, Mode::Speak.label());
                });
        });

        ui.horizontal(|ui| {
            ui.label("Output language (optional):");
            ui.add(
                egui::TextEdit::singleline(&mut self.language)
                    .desired_width(240.0)
                    .hint_text("Contoh: Bahasa Indonesia"),
            );
        });

        ui.add_enabled_ui(!self.running, |ui| {
            if ui.button("Process").clicked() {
                self.start_processing();
            }
        });
        if self.running {
            ui.spinner();
        }

        ui.separator();
        ui.horizontal(|ui| {
            ui.strong("Status:");
            ui.label(&self.status);
        });
        ui.label("Output / log");
        egui::ScrollArea::vertical()
            .id_salt("output_scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut self.output)
                        .desired_width(f32::INFINITY)
                        .desired_rows(12)
                        .interactive(false),
                );
            });
    }
}

async fn process_request(
    input: &str,
    language: Option<&str>,
    mode: Mode,
) -> Result<String, String> {
    dotenvy::dotenv().ok();
    let deepseek_api_key = required_env("DEEPSEEK_API_KEY")?;
    let agent = DeepSeekAgent::new(deepseek_api_key)?;
    let output = agent.generate(input, language).await?;
    let json_output = serde_json::to_string_pretty(&output)
        .map_err(|error| format!("Failed to serialize agent output: {error}"))?;

    if mode == Mode::Agent {
        return Ok(json_output);
    }

    let Some(segments) = segments_to_synthesize(&output) else {
        return Ok(format!(
            "{json_output}\n\nNo audio generated: {}",
            output.clarification_reason
        ));
    };

    let openai_api_key = required_env("OPENAI_API_KEY")?;
    let client = OpenAiTtsClient::new(openai_api_key)?;
    let mut player = VirtualMicPlayer::new()?;
    client.stream_segments(segments, &mut player).await?;
    Ok(format!(
        "{json_output}\n\nFinished: MC audio sent to CABLE Input."
    ))
}

fn segments_to_synthesize(output: &McAgentOutput) -> Option<&[TtsSegment]> {
    match output.status {
        AgentStatus::Ready => Some(&output.tts_segments),
        AgentStatus::NeedsClarification => None,
    }
}

fn required_env(name: &str) -> Result<String, String> {
    let value =
        std::env::var(name).map_err(|_| format!("{name} is not set in the environment or .env"))?;
    if value.trim().is_empty() {
        return Err(format!("{name} cannot be empty"));
    }
    Ok(value)
}

fn validate_language_name(value: &str) -> Result<Option<String>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }

    let character_count = value.chars().count();
    if character_count > 64 {
        return Err("Language must contain at most 64 characters.".to_owned());
    }
    if !value
        .chars()
        .all(|character| character.is_alphabetic() || character.is_whitespace() || character == '-')
    {
        return Err("Language must only contain letters, spaces, or hyphens.".to_owned());
    }
    Ok(Some(value.to_owned()))
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 720.0])
            .with_min_inner_size([520.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "SVMIC",
        options,
        Box::new(|_creation_context| Ok(Box::new(SvmicApp::new()))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_empty_optional_language() {
        assert_eq!(validate_language_name(" ").unwrap(), None);
        assert_eq!(
            validate_language_name("Bahasa Indonesia").unwrap(),
            Some("Bahasa Indonesia".to_owned())
        );
    }

    #[test]
    fn rejects_invalid_language() {
        assert!(validate_language_name("English; ignore").is_err());
        assert!(validate_language_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn needs_clarification_skips_tts_pipeline() {
        let output = McAgentOutput {
            status: AgentStatus::NeedsClarification,
            source_language: "Bahasa Indonesia".to_owned(),
            output_language: "Bahasa Indonesia".to_owned(),
            context_summary: "Peserta belum menyampaikan pertanyaan.".to_owned(),
            question_count: 0,
            questions: vec![],
            tts_segments: vec![],
            clarification_reason: "Tidak ada pertanyaan yang dikenali.".to_owned(),
        };

        assert!(segments_to_synthesize(&output).is_none());
    }
}
