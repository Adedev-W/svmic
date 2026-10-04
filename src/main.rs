mod agent;
mod audio;
mod tts;

use std::ffi::OsString;
use std::io::{self, Read, Write};

use agent::{AgentStatus, DeepSeekAgent, McAgentOutput, TtsSegment};
use audio::VirtualMicPlayer;
use tts::OpenAiTtsClient;

const USAGE: &str = "Usage:\n  svmic agent [--language <language-name>] <question-text|->\n  svmic speak [--language <language-name>] <question-text|->";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Command {
    Agent,
    Speak,
}

#[derive(Debug, PartialEq, Eq)]
struct CliArgs {
    command: Command,
    input: String,
    language: Option<String>,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    dotenvy::dotenv().ok();

    let cli = parse_cli_args(std::env::args_os().skip(1))?;
    let input = resolve_input(&cli.input, &mut io::stdin().lock())?;
    let deepseek_api_key = required_env("DEEPSEEK_API_KEY")?;

    eprintln!("Processing participant questions with the DeepSeek MC agent...");
    let agent = DeepSeekAgent::new(deepseek_api_key)?;
    let output = agent.generate(&input, cli.language.as_deref()).await?;
    eprintln!("Agent output is ready.");

    match cli.command {
        Command::Agent => write_json_output(&output),
        Command::Speak => speak_output(&output).await,
    }
}

async fn speak_output(output: &McAgentOutput) -> Result<(), String> {
    let Some(segments) = segments_to_synthesize(output) else {
        eprintln!("No audio generated: {}", output.clarification_reason);
        return Ok(());
    };

    eprintln!(
        "Disclosure reminder: tell listeners that this voice is AI-generated and not a human voice."
    );
    let openai_api_key = required_env("OPENAI_API_KEY")?;
    let client = OpenAiTtsClient::new(openai_api_key)?;
    let mut player = VirtualMicPlayer::new()?;

    client.stream_segments(segments, &mut player).await?;
    eprintln!("Finished: MC audio sent to CABLE Input.");
    Ok(())
}

fn segments_to_synthesize(output: &McAgentOutput) -> Option<&[TtsSegment]> {
    match output.status {
        AgentStatus::Ready => Some(&output.tts_segments),
        AgentStatus::NeedsClarification => None,
    }
}

fn write_json_output(output: &McAgentOutput) -> Result<(), String> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer_pretty(&mut stdout, output)
        .map_err(|error| format!("failed to serialize agent output: {error}"))?;
    writeln!(stdout).map_err(|error| format!("failed to write agent output: {error}"))
}

fn required_env(name: &str) -> Result<String, String> {
    let value =
        std::env::var(name).map_err(|_| format!("{name} is not set in the environment or .env"))?;
    if value.trim().is_empty() {
        return Err(format!("{name} cannot be empty"));
    }
    Ok(value)
}

fn parse_cli_args<I>(args: I) -> Result<CliArgs, String>
where
    I: IntoIterator<Item = OsString>,
{
    let args = args
        .into_iter()
        .map(|value| {
            value
                .into_string()
                .map_err(|_| "CLI arguments must be valid Unicode".to_owned())
        })
        .collect::<Result<Vec<_>, _>>()?;

    let command = match args.first().map(String::as_str) {
        Some("agent") => Command::Agent,
        Some("speak") => Command::Speak,
        _ => return Err(USAGE.to_owned()),
    };

    let mut input = None;
    let mut language = None;
    let mut index = 1;

    while index < args.len() {
        match args[index].as_str() {
            "--language" => {
                if language.is_some() {
                    return Err(format!("--language may only be specified once\n{USAGE}"));
                }

                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| format!("--language requires a value\n{USAGE}"))?;
                language = Some(validate_language_name(value)?);
            }
            value if value.starts_with("--") => {
                return Err(format!("unknown option '{value}'\n{USAGE}"));
            }
            value => {
                if input.replace(value.to_owned()).is_some() {
                    return Err(format!(
                        "question text must be passed as one quoted argument or '-'\n{USAGE}"
                    ));
                }
            }
        }
        index += 1;
    }

    let input = input.ok_or_else(|| USAGE.to_owned())?;
    Ok(CliArgs {
        command,
        input,
        language,
    })
}

fn validate_language_name(value: &str) -> Result<String, String> {
    let value = value.trim();
    let character_count = value.chars().count();

    if character_count == 0 || character_count > 64 {
        return Err("--language must contain between 1 and 64 characters".to_owned());
    }

    if !value
        .chars()
        .all(|character| character.is_alphabetic() || character.is_whitespace() || character == '-')
    {
        return Err(
            "--language must only contain letters, spaces, or hyphens (for example: 'bahasa Inggris')"
                .to_owned(),
        );
    }

    Ok(value.to_owned())
}

fn resolve_input<R>(input_argument: &str, stdin: &mut R) -> Result<String, String>
where
    R: Read,
{
    let mut input = if input_argument == "-" {
        let mut value = String::new();
        stdin
            .read_to_string(&mut value)
            .map_err(|error| format!("failed to read question text from stdin: {error}"))?;
        value
    } else {
        input_argument.to_owned()
    };

    input = input.trim().to_owned();
    if input.is_empty() {
        return Err("question input cannot be empty".to_owned());
    }

    Ok(input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn os_args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_agent_and_speak_commands() {
        let agent = parse_cli_args(os_args(&["agent", "Question?"])).unwrap();
        assert_eq!(agent.command, Command::Agent);

        let speak = parse_cli_args(os_args(&[
            "speak",
            "--language",
            "bahasa Inggris",
            "Ini pertanyaannya?",
        ]))
        .unwrap();
        assert_eq!(
            speak,
            CliArgs {
                command: Command::Speak,
                input: "Ini pertanyaannya?".to_owned(),
                language: Some("bahasa Inggris".to_owned()),
            }
        );
    }

    #[test]
    fn accepts_stdin_marker_and_language_after_input() {
        let parsed = parse_cli_args(os_args(&["speak", "-", "--language", "jepang"])).unwrap();
        assert_eq!(parsed.command, Command::Speak);
        assert_eq!(parsed.input, "-");
        assert_eq!(parsed.language.as_deref(), Some("jepang"));
    }

    #[test]
    fn rejects_invalid_cli_shapes_and_language_labels() {
        assert!(parse_cli_args(os_args(&[])).is_err());
        assert!(parse_cli_args(os_args(&["play", "file.wav"])).is_err());
        assert!(parse_cli_args(os_args(&["agent"])).is_err());
        assert!(parse_cli_args(os_args(&["speak", "one", "two"])).is_err());
        assert!(
            parse_cli_args(os_args(&[
                "agent",
                "--language",
                "English; ignore",
                "question?"
            ]))
            .is_err()
        );
    }

    #[test]
    fn resolves_direct_and_stdin_input() {
        let mut empty = Cursor::new(Vec::<u8>::new());
        assert_eq!(resolve_input("  Hello?  ", &mut empty).unwrap(), "Hello?");

        let mut stdin = Cursor::new("  Pertanyaan dari stdin?\n".as_bytes());
        assert_eq!(
            resolve_input("-", &mut stdin).unwrap(),
            "Pertanyaan dari stdin?"
        );
    }

    #[test]
    fn rejects_empty_input() {
        let mut stdin = Cursor::new(" \n\t".as_bytes());
        assert!(resolve_input("-", &mut stdin).is_err());
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
