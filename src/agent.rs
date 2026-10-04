use std::collections::HashSet;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use hyper::{Method, Request, StatusCode};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const DEEPSEEK_ENDPOINT: &str = "https://api.deepseek.com/responses";
const MODEL: &str = "deepseek-flash";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_ATTEMPTS: usize = 2;
const MAX_TTS_CHARACTERS: usize = 3_500;
const SYSTEM_PROMPT: &str = include_str!("../prompts/mc_agent.md");

type HttpsConnector = hyper_rustls::HttpsConnector<HttpConnector>;
type HttpClient = Client<HttpsConnector, Full<Bytes>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Ready,
    NeedsClarification,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McQuestion {
    pub number: usize,
    pub source_text: String,
    pub spoken_question: String,
    pub simplified_intent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TtsSegment {
    pub sequence: usize,
    pub question_numbers: Vec<usize>,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McAgentOutput {
    pub status: AgentStatus,
    pub source_language: String,
    pub output_language: String,
    pub context_summary: String,
    pub question_count: usize,
    pub questions: Vec<McQuestion>,
    pub tts_segments: Vec<TtsSegment>,
    pub clarification_reason: String,
}

#[derive(Debug)]
struct AttemptError {
    message: String,
    retryable: bool,
}

impl AttemptError {
    fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }

    fn terminal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }
}

pub struct DeepSeekAgent {
    client: HttpClient,
    api_key: String,
}

impl DeepSeekAgent {
    pub fn new(api_key: String) -> Result<Self, String> {
        if api_key.trim().is_empty() {
            return Err("DEEPSEEK_API_KEY cannot be empty".to_owned());
        }

        let https = HttpsConnectorBuilder::new()
            .with_native_roots()
            .map_err(|error| format!("failed to load native TLS certificates: {error}"))?
            .https_only()
            .enable_http1()
            .build();
        let client = Client::builder(TokioExecutor::new()).build(https);

        Ok(Self { client, api_key })
    }

    pub async fn generate(
        &self,
        participant_input: &str,
        language_override: Option<&str>,
    ) -> Result<McAgentOutput, String> {
        let mut last_error = None;

        for attempt in 1..=MAX_ATTEMPTS {
            match self
                .generate_once(participant_input, language_override)
                .await
            {
                Ok(output) => return Ok(output),
                Err(error) if error.retryable && attempt < MAX_ATTEMPTS => {
                    eprintln!(
                        "DeepSeek attempt {attempt}/{MAX_ATTEMPTS} failed; retrying: {}",
                        error.message
                    );
                    last_error = Some(error.message);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(error) => return Err(error.message),
            }
        }

        Err(last_error.unwrap_or_else(|| "DeepSeek request failed".to_owned()))
    }

    async fn generate_once(
        &self,
        participant_input: &str,
        language_override: Option<&str>,
    ) -> Result<McAgentOutput, AttemptError> {
        let payload = build_request_payload(participant_input, language_override);
        let body = serde_json::to_vec(&payload).map_err(|error| {
            AttemptError::terminal(format!("failed to serialize DeepSeek request: {error}"))
        })?;

        let request = Request::builder()
            .method(Method::POST)
            .uri(DEEPSEEK_ENDPOINT)
            .header(CONTENT_TYPE, "application/json")
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(USER_AGENT, concat!("svmic/", env!("CARGO_PKG_VERSION")))
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| {
                AttemptError::terminal(format!("failed to build DeepSeek request: {error}"))
            })?;

        let response = tokio::time::timeout(REQUEST_TIMEOUT, self.client.request(request))
            .await
            .map_err(|_| AttemptError::retryable("DeepSeek request timed out"))?
            .map_err(|error| {
                AttemptError::retryable(format!("DeepSeek connection failed: {error}"))
            })?;
        let status = response.status();
        let response_body = tokio::time::timeout(REQUEST_TIMEOUT, response.into_body().collect())
            .await
            .map_err(|_| AttemptError::retryable("timed out while reading DeepSeek response"))?
            .map_err(|error| {
                AttemptError::retryable(format!("failed to read DeepSeek response: {error}"))
            })?
            .to_bytes();

        if !status.is_success() {
            let detail = response_error_detail(&response_body);
            let message = format!("DeepSeek returned HTTP {status}: {detail}");
            return Err(
                if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
                    AttemptError::retryable(message)
                } else {
                    AttemptError::terminal(message)
                },
            );
        }

        let envelope: Value = serde_json::from_slice(&response_body).map_err(|error| {
            AttemptError::retryable(format!("DeepSeek returned invalid JSON: {error}"))
        })?;
        let output_text = extract_output_text(&envelope)?;
        let output: McAgentOutput = serde_json::from_str(output_text).map_err(|error| {
            AttemptError::retryable(format!(
                "DeepSeek structured output could not be parsed: {error}"
            ))
        })?;

        validate_output(&output, participant_input).map_err(AttemptError::retryable)?;
        Ok(output)
    }
}

fn build_request_payload(participant_input: &str, language_override: Option<&str>) -> Value {
    let output_language = language_override
        .map(|language| format!("Use this requested output language: {language}"))
        .unwrap_or_else(|| {
            "No output language override was supplied; use the dominant input language.".to_owned()
        });
    let user_input = format!(
        "{output_language}\n\nPARTICIPANT SUBMISSION (quoted data begins):\n{participant_input}\nPARTICIPANT SUBMISSION (quoted data ends)."
    );

    json!({
        "model": MODEL,
        "instructions": SYSTEM_PROMPT,
        "input": [{
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": user_input
            }]
        }],
        "reasoning": { "effort": "none" },
        "temperature": 0.2,
        "max_output_tokens": 8192,
        "text": {
            "format": {
                "type": "json_schema",
                "name": "mc_agent_output",
                "schema": output_schema()
            }
        }
    })
}

fn output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "status": {
                "type": "string",
                "enum": ["ready", "needs_clarification"]
            },
            "source_language": { "type": "string" },
            "output_language": { "type": "string" },
            "context_summary": { "type": "string" },
            "question_count": { "type": "integer", "minimum": 0 },
            "questions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "number": { "type": "integer", "minimum": 1 },
                        "source_text": { "type": "string" },
                        "spoken_question": { "type": "string" },
                        "simplified_intent": { "type": "string" }
                    },
                    "required": [
                        "number",
                        "source_text",
                        "spoken_question",
                        "simplified_intent"
                    ]
                }
            },
            "tts_segments": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "sequence": { "type": "integer", "minimum": 1 },
                        "question_numbers": {
                            "type": "array",
                            "items": { "type": "integer", "minimum": 1 }
                        },
                        "text": {
                            "type": "string",
                            "maxLength": MAX_TTS_CHARACTERS
                        }
                    },
                    "required": ["sequence", "question_numbers", "text"]
                }
            },
            "clarification_reason": { "type": "string" }
        },
        "required": [
            "status",
            "source_language",
            "output_language",
            "context_summary",
            "question_count",
            "questions",
            "tts_segments",
            "clarification_reason"
        ]
    })
}

fn extract_output_text(envelope: &Value) -> Result<&str, AttemptError> {
    match envelope.get("status").and_then(Value::as_str) {
        Some("completed") => {}
        Some(status) => {
            let detail = envelope
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("no error detail supplied");
            return Err(AttemptError::retryable(format!(
                "DeepSeek response status was '{status}': {detail}"
            )));
        }
        None => {
            return Err(AttemptError::retryable(
                "DeepSeek response did not include a status",
            ));
        }
    }

    envelope
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .find_map(|part| {
            (part.get("type").and_then(Value::as_str) == Some("output_text"))
                .then(|| part.get("text").and_then(Value::as_str))
                .flatten()
                .filter(|text| !text.trim().is_empty())
        })
        .ok_or_else(|| AttemptError::retryable("DeepSeek response contained no output text"))
}

fn validate_output(output: &McAgentOutput, participant_input: &str) -> Result<(), String> {
    require_non_empty("source_language", &output.source_language)?;
    require_non_empty("output_language", &output.output_language)?;
    require_non_empty("context_summary", &output.context_summary)?;

    match output.status {
        AgentStatus::Ready => validate_ready_output(output, participant_input),
        AgentStatus::NeedsClarification => {
            if output.question_count != 0
                || !output.questions.is_empty()
                || !output.tts_segments.is_empty()
            {
                return Err(
                    "needs_clarification output must have no questions or TTS segments".to_owned(),
                );
            }
            require_non_empty("clarification_reason", &output.clarification_reason)
        }
    }
}

fn validate_ready_output(output: &McAgentOutput, participant_input: &str) -> Result<(), String> {
    if output.question_count == 0 || output.questions.is_empty() {
        return Err("ready output must contain at least one question".to_owned());
    }
    if output.question_count != output.questions.len() {
        return Err(format!(
            "question_count is {}, but {} questions were returned",
            output.question_count,
            output.questions.len()
        ));
    }
    if output.tts_segments.is_empty() {
        return Err("ready output must contain at least one TTS segment".to_owned());
    }
    if !output.clarification_reason.trim().is_empty() {
        return Err("ready output must have an empty clarification_reason".to_owned());
    }

    for (index, question) in output.questions.iter().enumerate() {
        let expected_number = index + 1;
        if question.number != expected_number {
            return Err(format!(
                "question numbers must be contiguous; expected {expected_number}, got {}",
                question.number
            ));
        }
        require_non_empty("question.source_text", &question.source_text)?;
        require_non_empty("question.spoken_question", &question.spoken_question)?;
        require_non_empty("question.simplified_intent", &question.simplified_intent)?;
        if !participant_input.contains(question.source_text.trim()) {
            return Err(format!(
                "source_text for question {} is not an exact excerpt from the participant input",
                question.number
            ));
        }
    }

    let valid_numbers: HashSet<usize> = (1..=output.question_count).collect();
    let mut covered_numbers = HashSet::new();
    for (index, segment) in output.tts_segments.iter().enumerate() {
        let expected_sequence = index + 1;
        if segment.sequence != expected_sequence {
            return Err(format!(
                "TTS segment sequence must be contiguous; expected {expected_sequence}, got {}",
                segment.sequence
            ));
        }
        require_non_empty("tts_segment.text", &segment.text)?;
        let text_length = segment.text.chars().count();
        if text_length > MAX_TTS_CHARACTERS {
            return Err(format!(
                "TTS segment {} has {text_length} characters; maximum is {MAX_TTS_CHARACTERS}",
                segment.sequence
            ));
        }
        if segment.question_numbers.is_empty() {
            return Err(format!(
                "TTS segment {} must reference at least one question",
                segment.sequence
            ));
        }

        for number in &segment.question_numbers {
            if !valid_numbers.contains(number) {
                return Err(format!(
                    "TTS segment {} references unknown question {number}",
                    segment.sequence
                ));
            }
            if !covered_numbers.insert(*number) {
                return Err(format!(
                    "question {number} appears in more than one TTS segment"
                ));
            }
        }
    }

    if covered_numbers != valid_numbers {
        return Err("every question must appear in exactly one TTS segment".to_owned());
    }

    Ok(())
}

fn require_non_empty(field: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        Err(format!("{field} cannot be empty"))
    } else {
        Ok(())
    }
}

fn response_error_detail(body: &[u8]) -> String {
    let body = &body[..body.len().min(1_000)];
    let parsed: Result<Value, _> = serde_json::from_slice(body);
    parsed
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const INPUT: &str =
        "Saya mau tahu, kapan acaranya dimulai? Terus apakah peserta harus daftar ulang?";

    fn ready_output() -> McAgentOutput {
        McAgentOutput {
            status: AgentStatus::Ready,
            source_language: "Bahasa Indonesia".to_owned(),
            output_language: "Bahasa Indonesia".to_owned(),
            context_summary: "Peserta menanyakan jadwal dan registrasi acara.".to_owned(),
            question_count: 2,
            questions: vec![
                McQuestion {
                    number: 1,
                    source_text: "kapan acaranya dimulai?".to_owned(),
                    spoken_question: "Kapan acara dimulai?".to_owned(),
                    simplified_intent: "Peserta ingin mengetahui waktu mulai acara.".to_owned(),
                },
                McQuestion {
                    number: 2,
                    source_text: "apakah peserta harus daftar ulang?".to_owned(),
                    spoken_question: "Apakah peserta harus mendaftar ulang?".to_owned(),
                    simplified_intent: "Peserta ingin memastikan kewajiban registrasi ulang."
                        .to_owned(),
                },
            ],
            tts_segments: vec![TtsSegment {
                sequence: 1,
                question_numbers: vec![1, 2],
                text: "Di sini ada dua pertanyaan. Pertanyaan pertama mengenai waktu mulai acara. Pertanyaan kedua mengenai registrasi ulang peserta.".to_owned(),
            }],
            clarification_reason: String::new(),
        }
    }

    #[test]
    fn request_uses_deepseek_json_schema_and_language_override() {
        let payload = build_request_payload(INPUT, Some("bahasa Jepang"));
        assert_eq!(payload["model"], MODEL);
        assert_eq!(payload["reasoning"]["effort"], "none");
        assert_eq!(payload["text"]["format"]["type"], "json_schema");
        assert_eq!(
            payload["text"]["format"]["schema"]["properties"]["tts_segments"]["items"]["properties"]
                ["text"]["maxLength"],
            MAX_TTS_CHARACTERS
        );
        assert!(
            payload["input"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("bahasa Jepang")
        );
    }

    #[test]
    fn extracts_and_deserializes_responses_api_output() {
        let expected = ready_output();
        let envelope = json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{
                    "type": "output_text",
                    "text": serde_json::to_string(&expected).unwrap()
                }]
            }]
        });

        let parsed: McAgentOutput =
            serde_json::from_str(extract_output_text(&envelope).unwrap()).unwrap();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn validates_ready_output_and_question_coverage() {
        let output = ready_output();
        validate_output(&output, INPUT).unwrap();

        let mut missing = output.clone();
        missing.tts_segments[0].question_numbers = vec![1];
        assert!(validate_output(&missing, INPUT).is_err());

        let mut duplicate = output;
        duplicate.tts_segments.push(TtsSegment {
            sequence: 2,
            question_numbers: vec![2],
            text: "Pertanyaan kedua.".to_owned(),
        });
        assert!(validate_output(&duplicate, INPUT).is_err());
    }

    #[test]
    fn rejects_changed_source_text_and_overlong_tts_segment() {
        let mut output = ready_output();
        output.questions[0].source_text = "Pertanyaan yang tidak pernah dikirim.".to_owned();
        assert!(validate_output(&output, INPUT).is_err());

        let mut output = ready_output();
        output.tts_segments[0].text = "a".repeat(MAX_TTS_CHARACTERS + 1);
        assert!(validate_output(&output, INPUT).is_err());
    }

    #[test]
    fn validates_needs_clarification_output() {
        let output = McAgentOutput {
            status: AgentStatus::NeedsClarification,
            source_language: "Bahasa Indonesia".to_owned(),
            output_language: "Bahasa Indonesia".to_owned(),
            context_summary: "Peserta menyampaikan komentar tanpa pertanyaan.".to_owned(),
            question_count: 0,
            questions: vec![],
            tts_segments: vec![],
            clarification_reason: "Tidak ada pertanyaan yang dapat dikenali.".to_owned(),
        };
        validate_output(&output, "Terima kasih atas acaranya.").unwrap();
    }

    #[test]
    fn validates_single_question_fixture() {
        let input = "Kapan pendaftaran ditutup?";
        let output = McAgentOutput {
            status: AgentStatus::Ready,
            source_language: "Bahasa Indonesia".to_owned(),
            output_language: "Bahasa Indonesia".to_owned(),
            context_summary: "Peserta menanyakan tenggat pendaftaran.".to_owned(),
            question_count: 1,
            questions: vec![McQuestion {
                number: 1,
                source_text: input.to_owned(),
                spoken_question: input.to_owned(),
                simplified_intent: "Peserta ingin mengetahui tenggat pendaftaran.".to_owned(),
            }],
            tts_segments: vec![TtsSegment {
                sequence: 1,
                question_numbers: vec![1],
                text: "Ada satu pertanyaan. Kapan pendaftaran ditutup?".to_owned(),
            }],
            clarification_reason: String::new(),
        };

        validate_output(&output, input).unwrap();
    }

    #[test]
    fn validates_three_messy_questions_fixture() {
        let input =
            "eh mau tanya: mulai jam berapa? terus linknya mana ya? sama rekaman dibagi nggak?";
        let mut output = ready_output();
        output.context_summary = "Peserta menanyakan jadwal, tautan, dan rekaman.".to_owned();
        output.question_count = 3;
        output.questions = vec![
            McQuestion {
                number: 1,
                source_text: "mulai jam berapa?".to_owned(),
                spoken_question: "Acara dimulai pukul berapa?".to_owned(),
                simplified_intent: "Peserta ingin mengetahui waktu mulai acara.".to_owned(),
            },
            McQuestion {
                number: 2,
                source_text: "linknya mana ya?".to_owned(),
                spoken_question: "Tautan acaranya dapat ditemukan di mana?".to_owned(),
                simplified_intent: "Peserta ingin memperoleh tautan acara.".to_owned(),
            },
            McQuestion {
                number: 3,
                source_text: "rekaman dibagi nggak?".to_owned(),
                spoken_question: "Apakah rekaman acara akan dibagikan?".to_owned(),
                simplified_intent: "Peserta ingin memastikan rekaman akan dibagikan.".to_owned(),
            },
        ];
        output.tts_segments = vec![TtsSegment {
            sequence: 1,
            question_numbers: vec![1, 2, 3],
            text: "Ada tiga pertanyaan tentang jadwal, tautan, dan rekaman acara.".to_owned(),
        }];

        validate_output(&output, input).unwrap();
    }

    #[test]
    fn validates_long_context_fixture() {
        let context = "Kami sudah mengikuti rangkaian acara sejak pagi dan memahami penjelasan umum panitia. ".repeat(40);
        let question = "Apakah sertifikat dikirim melalui email?";
        let input = format!("{context}{question}");
        let mut output = ready_output();
        output.context_summary =
            "Peserta memberi konteks panjang lalu menanyakan pengiriman sertifikat.".to_owned();
        output.question_count = 1;
        output.questions = vec![McQuestion {
            number: 1,
            source_text: question.to_owned(),
            spoken_question: question.to_owned(),
            simplified_intent: "Peserta ingin mengetahui cara pengiriman sertifikat.".to_owned(),
        }];
        output.tts_segments = vec![TtsSegment {
            sequence: 1,
            question_numbers: vec![1],
            text: "Pertanyaannya, apakah sertifikat dikirim melalui email?".to_owned(),
        }];

        validate_output(&output, &input).unwrap();
    }

    #[test]
    fn rejects_empty_narrative_fields_fixture() {
        let mut output = ready_output();
        output.context_summary = "  ".to_owned();
        assert!(validate_output(&output, INPUT).is_err());
    }

    #[test]
    fn rejects_invalid_ready_counts_and_sequences() {
        let mut output = ready_output();
        output.question_count = 3;
        assert!(validate_output(&output, INPUT).is_err());

        let mut output = ready_output();
        output.questions[1].number = 3;
        assert!(validate_output(&output, INPUT).is_err());

        let mut output = ready_output();
        output.tts_segments[0].sequence = 2;
        assert!(validate_output(&output, INPUT).is_err());
    }

    #[tokio::test]
    #[ignore = "uses the live DeepSeek API and DEEPSEEK_API_KEY"]
    async fn live_smoke_test() {
        dotenvy::dotenv().ok();
        let api_key = std::env::var("DEEPSEEK_API_KEY").unwrap();
        let agent = DeepSeekAgent::new(api_key).unwrap();
        let output = agent.generate(INPUT, None).await.unwrap();
        assert_eq!(output.status, AgentStatus::Ready);
        assert_eq!(output.question_count, 2);
    }
}
