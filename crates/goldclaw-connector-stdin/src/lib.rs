use std::sync::Arc;

use async_trait::async_trait;
use goldclaw_core::{
    AssistantEvent, Connector, Envelope, EnvelopeSource, GoldClawError, Result, RuntimeHandle,
};
use tracing::debug;

/// Reads user input from stdin line-by-line, submits each line to the runtime
/// as a [`EnvelopeSource::Cli`] message, and prints the assistant reply before
/// prompting for the next message.
///
/// A single session is created at startup and reused for the whole interaction,
/// so conversation history is preserved across turns.
pub struct StdinConnector {
    session_title: String,
}

impl StdinConnector {
    pub fn new(session_title: impl Into<String>) -> Self {
        Self {
            session_title: session_title.into(),
        }
    }
}

impl Default for StdinConnector {
    fn default() -> Self {
        Self::new("stdin")
    }
}

#[async_trait]
impl Connector for StdinConnector {
    fn name(&self) -> &'static str {
        "stdin"
    }

    async fn run(self: Box<Self>, runtime: Arc<dyn RuntimeHandle>) -> Result<()> {
        let session = runtime
            .create_session(Some(self.session_title.clone()))
            .await?;
        let session_id = session.id;
        debug!(%session_id, "stdin connector started");

        println!("GoldClaw ({session_id})");
        println!("Type a message and press Enter. Ctrl-D or 'exit' to quit.\n");

        loop {
            // Prompt
            print!("> ");
            use std::io::Write;
            std::io::stdout()
                .flush()
                .map_err(|e| GoldClawError::Io(e.to_string()))?;

            // Read one line from stdin on a blocking thread so we don't stall the runtime.
            let line = tokio::task::spawn_blocking(|| -> std::io::Result<Option<String>> {
                let mut buf = String::new();
                match std::io::stdin().read_line(&mut buf) {
                    Ok(0) => Ok(None), // EOF
                    Ok(_) => Ok(Some(buf)),
                    Err(e) => Err(e),
                }
            })
            .await
            .map_err(|e| GoldClawError::Internal(e.to_string()))?
            .map_err(|e| GoldClawError::Io(e.to_string()))?;

            let Some(line) = line else {
                println!();
                break;
            };

            let content = line.trim().to_string();
            if content.is_empty() {
                continue;
            }
            if content == "exit" || content == "quit" {
                break;
            }

            // Subscribe before submit so we don't miss any events.
            let mut rx = runtime.subscribe(session_id).await?;

            let envelope = Envelope::user(content, EnvelopeSource::Cli, Some(session_id));
            runtime.submit(envelope).await?;

            // Wait for the assistant's reply.
            loop {
                match rx.recv().await {
                    Ok(AssistantEvent::MessageCompleted { content, .. }) => {
                        println!("\n{content}\n");
                        break;
                    }
                    Ok(AssistantEvent::Error { message, .. }) => {
                        eprintln!("error: {message}\n");
                        break;
                    }
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }

        println!("Goodbye.");
        Ok(())
    }
}
