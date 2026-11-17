use super::*;
use crate::openai::{client, request::ReplayItems};

impl Gateway {
    pub(super) async fn openai_chat<F>(
        &self,
        mut call: Call<'_>,
        payload: &Value,
        history: Option<&oxa_proto::ChatRequest<'_>>,
        cancel: &Cancellation,
        on: &mut F,
    ) -> Result<Completed, Error>
    where
        F: FnMut(Event) -> Control + ?Sized,
    {
        let model = call.model();
        let auth = call.openai_auth_handle().map_err(Error::Refused)?;
        let replay: Vec<_> = history
            .into_iter()
            .flat_map(|request| request.messages)
            .filter(|message| message.role == oxa_proto::Role::Assistant)
            .map(|message| {
                message
                    .provider_history
                    .as_ref()
                    .map(|saved| ReplayItems::new(saved.items().to_vec()))
            })
            .collect();
        let chat = client::Chat {
            base_url: model.endpoint().as_str(),
            upstream_model: model.upstream(),
            alias: model.info().id,
            max_output: Some(call.max_output()),
            payload,
            replay: &replay,
            created: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |t| t.as_secs()),
        };
        let mut terminal_usage = None;
        let result = self
            .openai
            .stream_with_usage(
                chat,
                auth.as_auth(),
                cancel,
                &mut |event| match event {
                    client::Event::Sending => {
                        if cancel.is_cancelled() {
                            return client::Control::Stop;
                        }
                        call.mark_forwarded();
                        client::Control::Continue
                    }
                    client::Event::Chunk(chunk) => {
                        call.observe(&chunk);
                        match on(Event::Chunk(chunk)) {
                            Control::Continue => client::Control::Continue,
                            Control::Stop => client::Control::Stop,
                        }
                    }
                },
                &mut terminal_usage,
            )
            .await
            .map(|completed| Completed {
                assistant: completed.assistant,
                usage: completed.usage,
                output_items: completed.output_items,
                replay_items: completed.replay_items,
                finish_reason: completed.finish_reason,
                history: None,
            })
            .map_err(Error::OpenAi);
        let usage_final = terminal_usage.is_some();
        if let Some(usage) = terminal_usage {
            call.metered_usage(usage);
        }
        self.finish(call, result, history, usage_final, on)
    }
}
