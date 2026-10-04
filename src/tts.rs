use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use hyper::{Method, Request, StatusCode};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};

use crate::agent::TtsSegment;
use crate::audio::VirtualMicPlayer;

const OPENAI_SPEECH_ENDPOINT: &str = "https://api.openai.com/v1/audio/speech";
const MODEL: &str = "gpt-4o-mini-tts";
const VOICE: &str = "marin";
const VOICE_INSTRUCTIONS: &str = "Speak like a professional master of ceremonies. Be clear, natural, neutral, and concise. Preserve the language of the supplied text and use comfortable pacing.";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(45);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(45);
const MAX_ATTEMPTS: usize = 2;

type HttpsConnector = hyper_rustls::HttpsConnector<HttpConnector>;
type HttpClient = Client<HttpsConnector, Full<Bytes>>;

#[derive(Debug)]
struct TtsAttemptError {
    message: String,
    retryable: bool,
}

impl TtsAttemptError {
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

    fn based_on_audio_progress(message: impl Into<String>, audio_started: bool) -> Self {
        if audio_started {
            Self::terminal(message)
        } else {
            Self::retryable(message)
        }
    }
}

pub struct OpenAiTtsClient {
    client: HttpClient,
    api_key: String,
}

impl OpenAiTtsClient {
    pub fn new(api_key: String) -> Result<Self, String> {
        if api_key.trim().is_empty() {
            return Err("OPENAI_API_KEY cannot be empty".to_owned());
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

    pub async fn stream_segments(
        &self,
        segments: &[TtsSegment],
        player: &mut VirtualMicPlayer,
    ) -> Result<(), String> {
        if segments.is_empty() {
            return Err("ready agent output contains no TTS segments".to_owned());
        }

        for segment in segments {
            eprintln!(
                "Synthesizing TTS segment {}/{}...",
                segment.sequence,
                segments.len()
            );
            self.stream_segment_with_retry(&segment.text, player)
                .await?;
        }

        player.finish().await
    }

    async fn stream_segment_with_retry(
        &self,
        text: &str,
        player: &mut VirtualMicPlayer,
    ) -> Result<(), String> {
        for attempt in 1..=MAX_ATTEMPTS {
            match self.stream_segment_once(text, player).await {
                Ok(()) => return Ok(()),
                Err(error) if error.retryable && attempt < MAX_ATTEMPTS => {
                    eprintln!(
                        "OpenAI TTS attempt {attempt}/{MAX_ATTEMPTS} failed; retrying: {}",
                        error.message
                    );
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(error) => return Err(error.message),
            }
        }
        Err("OpenAI TTS request failed".to_owned())
    }

    async fn stream_segment_once(
        &self,
        text: &str,
        player: &mut VirtualMicPlayer,
    ) -> Result<(), TtsAttemptError> {
        let payload = build_request_payload(text);
        let body = serde_json::to_vec(&payload).map_err(|error| {
            TtsAttemptError::terminal(format!("failed to serialize OpenAI TTS request: {error}"))
        })?;
        let request = Request::builder()
            .method(Method::POST)
            .uri(OPENAI_SPEECH_ENDPOINT)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/octet-stream")
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(USER_AGENT, concat!("svmic/", env!("CARGO_PKG_VERSION")))
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| {
                TtsAttemptError::terminal(format!("failed to build OpenAI TTS request: {error}"))
            })?;

        let mut response = tokio::time::timeout(REQUEST_TIMEOUT, self.client.request(request))
            .await
            .map_err(|_| TtsAttemptError::retryable("OpenAI TTS request timed out"))?
            .map_err(|error| {
                TtsAttemptError::retryable(format!("OpenAI TTS connection failed: {error}"))
            })?;
        let status = response.status();

        if !status.is_success() {
            let body = tokio::time::timeout(REQUEST_TIMEOUT, response.into_body().collect())
                .await
                .map_err(|_| {
                    TtsAttemptError::retryable("timed out while reading OpenAI TTS error response")
                })?
                .map_err(|error| {
                    TtsAttemptError::retryable(format!(
                        "failed to read OpenAI TTS error response: {error}"
                    ))
                })?
                .to_bytes();
            let message = format!(
                "OpenAI TTS returned HTTP {status}: {}",
                response_error_detail(&body)
            );
            return Err(if is_retryable_status(status) {
                TtsAttemptError::retryable(message)
            } else {
                TtsAttemptError::terminal(message)
            });
        }

        let mut decoder = Pcm16LeDecoder::default();
        let mut audio_started = false;
        loop {
            let frame = tokio::time::timeout(CHUNK_TIMEOUT, response.body_mut().frame())
                .await
                .map_err(|_| {
                    TtsAttemptError::based_on_audio_progress(
                        "timed out while streaming OpenAI TTS audio",
                        audio_started,
                    )
                })?;
            let Some(frame) = frame else {
                break;
            };
            let frame = frame.map_err(|error| {
                TtsAttemptError::based_on_audio_progress(
                    format!("failed while streaming OpenAI TTS audio: {error}"),
                    audio_started,
                )
            })?;
            let Ok(data) = frame.into_data() else {
                continue;
            };
            let samples = decoder.push(&data);
            if !samples.is_empty() {
                player
                    .push_pcm_samples(&samples)
                    .await
                    .map_err(TtsAttemptError::terminal)?;
                audio_started = true;
            }
        }

        decoder
            .finish()
            .map_err(|message| TtsAttemptError::based_on_audio_progress(message, audio_started))?;
        if !audio_started {
            return Err(TtsAttemptError::retryable(
                "OpenAI TTS returned an empty audio stream",
            ));
        }
        Ok(())
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn build_request_payload(text: &str) -> Value {
    json!({
        "model": MODEL,
        "voice": VOICE,
        "input": text,
        "instructions": VOICE_INSTRUCTIONS,
        "response_format": "pcm",
        "stream_format": "audio"
    })
}

#[derive(Debug, Default)]
struct Pcm16LeDecoder {
    pending_byte: Option<u8>,
    sample_count: usize,
}

impl Pcm16LeDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<i16> {
        let mut samples =
            Vec::with_capacity((bytes.len() + usize::from(self.pending_byte.is_some())) / 2);
        let mut index = 0;

        if let Some(low) = self.pending_byte.take() {
            if let Some(&high) = bytes.first() {
                samples.push(i16::from_le_bytes([low, high]));
                index = 1;
            } else {
                self.pending_byte = Some(low);
                return samples;
            }
        }

        while index + 1 < bytes.len() {
            samples.push(i16::from_le_bytes([bytes[index], bytes[index + 1]]));
            index += 2;
        }
        if index < bytes.len() {
            self.pending_byte = Some(bytes[index]);
        }
        self.sample_count += samples.len();
        samples
    }

    fn finish(self) -> Result<usize, String> {
        if self.pending_byte.is_some() {
            return Err("OpenAI TTS PCM stream ended with an incomplete 16-bit sample".to_owned());
        }
        if self.sample_count == 0 {
            return Err("OpenAI TTS returned an empty PCM stream".to_owned());
        }
        Ok(self.sample_count)
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

    #[test]
    fn request_payload_uses_streaming_pcm_and_marin() {
        let payload = build_request_payload("Selamat datang.");
        assert_eq!(payload["model"], "gpt-4o-mini-tts");
        assert_eq!(payload["voice"], "marin");
        assert_eq!(payload["input"], "Selamat datang.");
        assert_eq!(payload["response_format"], "pcm");
        assert_eq!(payload["stream_format"], "audio");
        assert!(
            payload["instructions"]
                .as_str()
                .unwrap()
                .contains("master of ceremonies")
        );
    }

    #[test]
    fn decoder_reads_even_pcm_chunks() {
        let mut decoder = Pcm16LeDecoder::default();
        assert_eq!(decoder.push(&[0x34, 0x12, 0x00, 0x80]), [0x1234, i16::MIN]);
        assert_eq!(decoder.finish().unwrap(), 2);
    }

    #[test]
    fn decoder_carries_odd_byte_across_chunks() {
        let mut decoder = Pcm16LeDecoder::default();
        assert!(decoder.push(&[0x34]).is_empty());
        assert_eq!(decoder.push(&[0x12, 0xFE, 0xFF]), [0x1234, -2]);
        assert_eq!(decoder.finish().unwrap(), 2);
    }

    #[test]
    fn decoder_rejects_empty_and_incomplete_streams() {
        assert!(Pcm16LeDecoder::default().finish().is_err());

        let mut incomplete = Pcm16LeDecoder::default();
        assert!(incomplete.push(&[1]).is_empty());
        assert!(incomplete.finish().is_err());
    }

    #[test]
    fn error_detail_extracts_api_message_without_secrets() {
        let body = br#"{"error":{"message":"invalid voice"}}"#;
        assert_eq!(response_error_detail(body), "invalid voice");
    }

    #[test]
    fn retries_rate_limits_and_server_errors_only() {
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));

        assert!(TtsAttemptError::based_on_audio_progress("network", false).retryable);
        assert!(!TtsAttemptError::based_on_audio_progress("network", true).retryable);
    }
}
