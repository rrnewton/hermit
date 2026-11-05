//! Adapts the ElevenLabs audio client to the shared read-aloud interface.

use std::sync::Arc;

use async_trait::async_trait;
use futures_util::StreamExt as _;

use crate::config::ElevenLabsConfig;
use crate::speech::{Description, Playback, Speech, SpeechError, SpeechProvider, SpeechStream};

/// ElevenLabs read-aloud with its configuration held behind the provider boundary.
pub struct ElevenLabsSpeech {
    client: Arc<dyn super::SpeechProvider>,
    config: ElevenLabsConfig,
}

impl ElevenLabsSpeech {
    /// Use the given client's connection pool and the deployment's speech settings.
    #[must_use]
    pub fn new(client: Arc<dyn super::SpeechProvider>, config: ElevenLabsConfig) -> Self {
        Self { client, config }
    }
}

fn shared_error(error: super::SpeechError) -> SpeechError {
    let code = error.code();
    let detail = error.to_string();
    match error {
        super::SpeechError::NotConfigured(_) => SpeechError::NotConfigured { code, detail },
        super::SpeechError::Empty => SpeechError::Empty,
        super::SpeechError::Transport(_) | super::SpeechError::Status { .. } => {
            SpeechError::Backend { code, detail }
        }
    }
}

#[async_trait]
impl SpeechProvider for ElevenLabsSpeech {
    fn describe(&self) -> Description {
        Description {
            backend: "elevenlabs",
            label: "ElevenLabs".to_owned(),
            playback: Playback::Audio,
            local_only: false,
        }
    }

    async fn warm_up(&self) -> Result<(), SpeechError> {
        // The client resolves and caches the selected voice before checking for empty text.
        // This opens the reusable connection without asking ElevenLabs to synthesize anything.
        match self.client.speak(&self.config, "", None).await {
            Ok(_) | Err(super::SpeechError::Empty) => Ok(()),
            Err(error) => Err(shared_error(error)),
        }
    }

    async fn speak(&self, text: &str, speed: Option<f64>) -> Result<Speech, SpeechError> {
        self.client
            .speak(&self.config, text, speed)
            .await
            .map_err(shared_error)
    }

    async fn speak_stream(
        &self,
        text: &str,
        speed: Option<f64>,
    ) -> Result<SpeechStream, SpeechError> {
        let stream = self
            .client
            .speak_stream(&self.config, text, speed)
            .await
            .map_err(shared_error)?;
        Ok(SpeechStream {
            content_type: stream.content_type,
            preamble: stream.preamble,
            session: stream.session,
            chunks: stream
                .chunks
                .map(|chunk| chunk.map_err(shared_error))
                .boxed(),
        })
    }
}
