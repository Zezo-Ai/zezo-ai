use anyhow::{anyhow, Result};
use editor::Editor;
use futures::AsyncBufReadExt;
use futures::{io::BufReader, AsyncReadExt, Stream, StreamExt};
use gpui::executor::Background;
use gpui::{actions, AppContext, Task, ViewContext};
use indoc::indoc;
use isahc::prelude::*;
use isahc::{http::StatusCode, Request};
use serde::{Deserialize, Serialize};
use std::{io, sync::Arc};
use util::ResultExt;

actions!(ai, [Assist]);

// Data types for chat completion requests
#[derive(Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<RequestMessage>,
    stream: bool,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
struct RequestMessage {
    role: Role,
    content: String,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
struct ResponseMessage {
    role: Option<Role>,
    content: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Role {
    User,
    Assistant,
    System,
}

#[derive(Deserialize, Debug)]
struct OpenAIResponseStreamEvent {
    pub id: Option<String>,
    pub object: String,
    pub created: u32,
    pub model: String,
    pub choices: Vec<ChatChoiceDelta>,
    pub usage: Option<Usage>,
}

#[derive(Deserialize, Debug)]
struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Deserialize, Debug)]
struct ChatChoiceDelta {
    pub index: u32,
    pub delta: ResponseMessage,
    pub finish_reason: Option<String>,
}

#[derive(Deserialize, Debug)]
struct OpenAIUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
}

#[derive(Deserialize, Debug)]
struct OpenAIChoice {
    text: String,
    index: u32,
    logprobs: Option<serde_json::Value>,
    finish_reason: Option<String>,
}

pub fn init(cx: &mut AppContext) {
    cx.add_async_action(assist)
}

fn assist(
    editor: &mut Editor,
    _: &Assist,
    cx: &mut ViewContext<Editor>,
) -> Option<Task<Result<()>>> {
    let api_key = std::env::var("OPENAI_API_KEY").log_err()?;

    const SYSTEM_MESSAGE: &'static str = indoc! {r#"
        You an AI language model embedded in a code editor named Zed, authored by Zed Industries.
        The input you are currently processing was produced by a special \"model mention\" in a document that is open in the editor.
        A model mention is indicated via a leading / on a line.
        The user's currently selected text is indicated via ->->selected text<-<- surrounding selected text.
        In this sentence, the word ->->example<-<- is selected.
        Respond to any selected model mention.
        Wrap your responses in > < as follows.
        >
        I think that's a great idea.
        <
        If you're responding to a distant mention or multiple mentions, provide context.
        > Key ideas of generative programming.
        * Managing context
            * Managing length
            * Context distillation
                - Shrink a context's size without loss of meaning.
        * Fine-grained version control
            * Portals to other contexts
                * Distillation policies
                * Budgets
        <

        > Expand on the idea of context distillation.
        It's important to stay below the model's context size when generative programming.
        A key technique in doing so is called context distillation... [up to 1 paragraph].

        Questions to consider:
        -
        -
        - [Up to 3 questions]
        <
    "#};

    let selections = editor.selections.all(cx);
    let (user_message, insertion_site) = editor.buffer().update(cx, |buffer, cx| {
        // Insert ->-> <-<- around selected text as described in the system prompt above.
        let snapshot = buffer.snapshot(cx);
        let mut user_message = String::new();
        let mut buffer_offset = 0;
        for selection in selections {
            user_message.extend(snapshot.text_for_range(buffer_offset..selection.start));
            user_message.push_str("->->");
            user_message.extend(snapshot.text_for_range(selection.start..selection.end));
            buffer_offset = selection.end;
            user_message.push_str("<-<-");
        }
        if buffer_offset < snapshot.len() {
            user_message.extend(snapshot.text_for_range(buffer_offset..snapshot.len()));
        }

        // Ensure the document ends with 4 trailing newlines.
        let trailing_newline_count = snapshot
            .reversed_chars_at(snapshot.len())
            .take_while(|c| *c == '\n')
            .take(4);
        let suffix = "\n".repeat(4 - trailing_newline_count.count());
        buffer.edit([(snapshot.len()..snapshot.len(), suffix)], None, cx);

        let snapshot = buffer.snapshot(cx); // Take a new snapshot after editing.
        let insertion_site = snapshot.anchor_after(snapshot.len() - 2);

        (user_message, insertion_site)
    });

    let stream = stream_completion(
        api_key,
        cx.background_executor().clone(),
        OpenAIRequest {
            model: "gpt-4".to_string(),
            messages: vec![
                RequestMessage {
                    role: Role::System,
                    content: SYSTEM_MESSAGE.to_string(),
                },
                RequestMessage {
                    role: Role::User,
                    content: user_message,
                },
            ],
            stream: false,
        },
    );
    let buffer = editor.buffer().clone();
    Some(cx.spawn(|_, mut cx| async move {
        let mut messages = stream.await?;
        while let Some(message) = messages.next().await {
            let mut message = message?;
            if let Some(choice) = message.choices.pop() {
                buffer.update(&mut cx, |buffer, cx| {
                    let text: Arc<str> = choice.delta.content?.into();
                    buffer.edit([(insertion_site.clone()..insertion_site, text)], None, cx);
                    Some(())
                });
            }
        }
        Ok(())
    }))
}

async fn stream_completion(
    api_key: String,
    executor: Arc<Background>,
    mut request: OpenAIRequest,
) -> Result<impl Stream<Item = Result<OpenAIResponseStreamEvent>>> {
    request.stream = true;

    let (tx, rx) = futures::channel::mpsc::unbounded::<Result<OpenAIResponseStreamEvent>>();

    let json_data = serde_json::to_string(&request)?;
    let mut response = Request::post("https://api.openai.com/v1/chat/completions")
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {}", api_key))
        .body(json_data)?
        .send_async()
        .await?;

    let status = response.status();
    if status == StatusCode::OK {
        executor
            .spawn(async move {
                let mut lines = BufReader::new(response.body_mut()).lines();

                fn parse_line(
                    line: Result<String, io::Error>,
                ) -> Result<Option<OpenAIResponseStreamEvent>> {
                    if let Some(data) = line?.strip_prefix("data: ") {
                        let event = serde_json::from_str(&data)?;
                        Ok(Some(event))
                    } else {
                        Ok(None)
                    }
                }

                while let Some(line) = lines.next().await {
                    if let Some(event) = parse_line(line).transpose() {
                        tx.unbounded_send(event).log_err();
                    }
                }

                anyhow::Ok(())
            })
            .detach();

        Ok(rx)
    } else {
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;

        Err(anyhow!(
            "Failed to connect to OpenAI API: {} {}",
            response.status(),
            body,
        ))
    }
}

#[cfg(test)]
mod tests {}
