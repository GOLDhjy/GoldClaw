use std::{
    cmp,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use goldclaw_config::{ConfigError, GoldClawConfig, ProjectPaths};
use goldclaw_core::{
    AssistantEvent, EnvelopeSource, MessageRole, RuntimeHealth, SessionDetail, SessionId,
    SessionMessage, SessionSummary, SubmissionReceipt,
};
use ratatui::{
    DefaultTerminal, Frame,
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, task::JoinHandle, time::sleep};

#[tokio::main]
async fn main() -> Result<()> {
    let (client, bind) = GatewayClient::from_local_config()?;
    let mut terminal = ratatui::try_init()?;
    let _terminal_guard = TerminalGuard;
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();

    let mut app = App::new(client, bind, event_tx);
    app.bootstrap().await;
    let outcome = app.run(&mut terminal, &mut event_rx).await;
    app.shutdown();
    outcome
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = ratatui::try_restore();
    }
}

#[derive(Clone)]
struct GatewayClient {
    http: Client,
    base_url: String,
}

impl GatewayClient {
    fn from_local_config() -> Result<(Self, String)> {
        let paths = ProjectPaths::discover()?;
        let config_path = paths.config_file();
        let config = GoldClawConfig::load_resolved(&config_path).map_err(|error| match error {
            ConfigError::MissingConfig(_) => {
                anyhow!("GoldClaw 尚未初始化，请先运行 `goldclaw init`。")
            }
            other => other.into(),
        })?;

        let bind = config.gateway.bind.clone();
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(2))
            .no_proxy()
            .build()
            .context("failed to build HTTP client")?;

        Ok((
            Self {
                http: client,
                base_url: format!("http://{bind}"),
            },
            bind,
        ))
    }

    async fn status(&self) -> Result<StatusResponse> {
        let response = self
            .http
            .get(format!("{}/status", self.base_url))
            .send()
            .await
            .context("failed to query gateway status")?;
        parse_json_response(response).await
    }

    async fn create_session(&self, title: Option<String>) -> Result<SessionSummary> {
        let response = self
            .http
            .post(format!("{}/sessions", self.base_url))
            .json(&CreateSessionRequest { title })
            .send()
            .await
            .context("failed to create session")?;
        parse_json_response(response).await
    }

    async fn load_session(&self, session_id: SessionId) -> Result<SessionDetail> {
        let response = self
            .http
            .get(format!("{}/sessions/{session_id}", self.base_url))
            .send()
            .await
            .with_context(|| format!("failed to load session {session_id}"))?;
        parse_json_response(response).await
    }

    async fn submit_message(
        &self,
        session_id: SessionId,
        content: String,
    ) -> Result<SubmissionReceipt> {
        let response = self
            .http
            .post(format!("{}/messages", self.base_url))
            .json(&SubmitMessageRequest {
                session_id: Some(session_id),
                content,
                source: Some(EnvelopeSource::Tui),
            })
            .send()
            .await
            .with_context(|| format!("failed to submit message for session {session_id}"))?;
        parse_json_response(response).await
    }

    async fn stream_session_events(
        &self,
        session_id: SessionId,
        tx: mpsc::UnboundedSender<NetworkEvent>,
    ) -> Result<()> {
        let response = self
            .http
            .get(format!("{}/sessions/{session_id}/events", self.base_url))
            .send()
            .await
            .with_context(|| format!("failed to connect event stream for session {session_id}"))?;

        if !response.status().is_success() {
            return Err(parse_http_error(response).await);
        }

        let _ = tx.send(NetworkEvent::Connected { session_id });

        let mut response = response;
        let mut buffer = Vec::new();
        let mut data_lines: Vec<String> = Vec::new();

        while let Some(chunk) = response
            .chunk()
            .await
            .context("failed to read event stream")?
        {
            buffer.extend_from_slice(&chunk);
            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let mut line = buffer.drain(..=newline).collect::<Vec<_>>();
                if matches!(line.last(), Some(b'\n')) {
                    line.pop();
                }
                if matches!(line.last(), Some(b'\r')) {
                    line.pop();
                }

                if line.is_empty() {
                    if !data_lines.is_empty() {
                        let payload = data_lines.join("\n");
                        let event = serde_json::from_str::<AssistantEvent>(&payload)
                            .context("failed to decode assistant event")?;
                        let _ = tx.send(NetworkEvent::Assistant(event));
                        data_lines.clear();
                    }
                    continue;
                }

                if line.first() == Some(&b':') {
                    continue;
                }

                let line = String::from_utf8_lossy(&line);
                if let Some(data) = line.strip_prefix("data:") {
                    data_lines.push(data.trim_start().to_string());
                }
            }
        }

        Ok(())
    }
}

async fn parse_json_response<T>(response: Response) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    if response.status().is_success() {
        response
            .json::<T>()
            .await
            .context("failed to decode gateway response")
    } else {
        Err(parse_http_error(response).await)
    }
}

async fn parse_http_error(response: Response) -> anyhow::Error {
    let status = response.status();
    match response.json::<ApiProblem>().await {
        Ok(problem) => anyhow!("gateway error {}: {}", status.as_u16(), problem.message),
        Err(_) => anyhow!("gateway request failed with status {}", status.as_u16()),
    }
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    health: RuntimeHealth,
    sessions: Vec<SessionSummary>,
}

#[derive(Debug, Deserialize)]
struct ApiProblem {
    message: String,
}

#[derive(Debug, Serialize)]
struct CreateSessionRequest {
    title: Option<String>,
}

#[derive(Debug, Serialize)]
struct SubmitMessageRequest {
    session_id: Option<SessionId>,
    content: String,
    source: Option<EnvelopeSource>,
}

enum NetworkEvent {
    Assistant(AssistantEvent),
    Connected {
        session_id: SessionId,
    },
    Disconnected {
        session_id: SessionId,
        error: String,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Sessions,
    Input,
}

struct App {
    client: GatewayClient,
    bind: String,
    sessions: Vec<SessionSummary>,
    selected_session: Option<SessionId>,
    transcript: Vec<TranscriptEntry>,
    input: String,
    focus: Focus,
    health: Option<RuntimeHealth>,
    status: String,
    show_help: bool,
    show_diagnostics: bool,
    should_quit: bool,
    next_refresh_due: Instant,
    refresh_requested: bool,
    ignore_completed_content: Option<String>,
    stream_task: Option<JoinHandle<()>>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
}

impl App {
    fn new(
        client: GatewayClient,
        bind: String,
        event_tx: mpsc::UnboundedSender<NetworkEvent>,
    ) -> Self {
        Self {
            client,
            bind,
            sessions: Vec::new(),
            selected_session: None,
            transcript: Vec::new(),
            input: String::new(),
            focus: Focus::Input,
            health: None,
            status: "正在连接 GoldClaw gateway...".into(),
            show_help: false,
            show_diagnostics: false,
            should_quit: false,
            next_refresh_due: Instant::now(),
            refresh_requested: false,
            ignore_completed_content: None,
            stream_task: None,
            event_tx,
        }
    }

    async fn bootstrap(&mut self) {
        match self.refresh_overview().await {
            Ok(()) => {
                if let Some(session_id) = self.selected_session {
                    if let Err(error) = self.load_selected_session(session_id).await {
                        self.status = format!("加载会话失败: {error}");
                    }
                } else {
                    self.status = "已连接。按 `n` 新建会话，输入消息后回车发送。".into();
                }
            }
            Err(error) => {
                self.status = format!("未连接到 gateway: {error}，按 `r` 重试。");
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(task) = self.stream_task.take() {
            task.abort();
        }
    }

    async fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        event_rx: &mut mpsc::UnboundedReceiver<NetworkEvent>,
    ) -> Result<()> {
        loop {
            while let Ok(event) = event_rx.try_recv() {
                self.handle_network_event(event);
            }

            if self.refresh_requested || Instant::now() >= self.next_refresh_due {
                if let Err(error) = self.refresh_overview().await {
                    self.status = format!("刷新失败: {error}");
                }
            }

            terminal.draw(|frame| self.render(frame))?;

            if self.should_quit {
                break;
            }

            if event::poll(Duration::from_millis(80))? {
                match event::read()? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        self.handle_key(key).await?;
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
        }

        Ok(())
    }

    async fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }

        if self.show_help || self.show_diagnostics {
            match key.code {
                KeyCode::Esc | KeyCode::Char('h') | KeyCode::Char('?') => {
                    self.show_help = false;
                    self.show_diagnostics = false;
                }
                KeyCode::Char('d') => {
                    self.show_diagnostics = !self.show_diagnostics;
                    if self.show_diagnostics {
                        self.show_help = false;
                    }
                }
                KeyCode::Char('q') => self.should_quit = true,
                _ => {}
            }
            return Ok(());
        }

        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab => {
                self.focus = match self.focus {
                    Focus::Sessions => Focus::Input,
                    Focus::Input => Focus::Sessions,
                };
            }
            KeyCode::Char('h') | KeyCode::Char('?') => self.show_help = true,
            KeyCode::Char('d') => {
                self.show_diagnostics = true;
                self.refresh_requested = true;
            }
            KeyCode::Char('r') => {
                self.refresh_requested = true;
                if let Some(session_id) = self.selected_session {
                    if let Err(error) = self.load_selected_session(session_id).await {
                        self.status = format!("重连失败: {error}");
                    }
                }
            }
            KeyCode::Char('n') => {
                self.create_empty_session().await?;
            }
            KeyCode::Up if self.focus == Focus::Sessions => {
                self.move_selection(-1).await?;
            }
            KeyCode::Down if self.focus == Focus::Sessions => {
                self.move_selection(1).await?;
            }
            KeyCode::Enter if self.focus == Focus::Input => {
                self.submit_input().await?;
            }
            KeyCode::Esc => {
                self.input.clear();
            }
            KeyCode::Backspace if self.focus == Focus::Input => {
                self.input.pop();
            }
            KeyCode::Char(ch)
                if self.focus == Focus::Input && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.input.push(ch);
            }
            _ => {}
        }

        Ok(())
    }

    async fn refresh_overview(&mut self) -> Result<()> {
        let status = self.client.status().await?;
        self.health = Some(status.health);
        self.sessions = status.sessions;
        self.sync_selection();
        self.refresh_requested = false;
        self.next_refresh_due = Instant::now() + Duration::from_secs(5);
        Ok(())
    }

    fn sync_selection(&mut self) {
        if self.sessions.is_empty() {
            self.selected_session = None;
            self.transcript.clear();
            return;
        }

        if let Some(session_id) = self.selected_session {
            if self.sessions.iter().any(|session| session.id == session_id) {
                return;
            }
        }

        self.selected_session = self.sessions.first().map(|session| session.id);
    }

    async fn move_selection(&mut self, delta: isize) -> Result<()> {
        if self.sessions.is_empty() {
            return Ok(());
        }

        let current = self
            .selected_session
            .and_then(|selected| {
                self.sessions
                    .iter()
                    .position(|session| session.id == selected)
            })
            .unwrap_or(0);
        let target = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            cmp::min(
                current + delta as usize,
                self.sessions.len().saturating_sub(1),
            )
        };

        let session_id = self.sessions[target].id;
        if Some(session_id) != self.selected_session {
            self.load_selected_session(session_id).await?;
        }
        Ok(())
    }

    async fn create_empty_session(&mut self) -> Result<()> {
        let session = self.client.create_session(None).await?;
        self.replace_or_insert_session(session.clone());
        self.load_selected_session(session.id).await?;
        self.status = "已创建新会话。输入消息后回车发送。".into();
        Ok(())
    }

    async fn ensure_selected_session(&mut self, first_message: &str) -> Result<SessionId> {
        if let Some(session_id) = self.selected_session {
            return Ok(session_id);
        }

        let title = summarize_title(first_message);
        let session = self.client.create_session(Some(title)).await?;
        self.replace_or_insert_session(session.clone());
        self.load_selected_session(session.id).await?;
        Ok(session.id)
    }

    async fn load_selected_session(&mut self, session_id: SessionId) -> Result<()> {
        let detail = self.client.load_session(session_id).await?;
        self.selected_session = Some(detail.session.id);
        self.replace_or_insert_session(detail.session.clone());
        self.transcript = detail
            .messages
            .iter()
            .map(transcript_from_message)
            .collect::<Vec<_>>();
        self.ignore_completed_content = None;
        self.start_stream(session_id);
        self.status = format!(
            "当前会话: {}。按 `Tab` 切换焦点，`?` 查看帮助。",
            detail.session.title
        );
        Ok(())
    }

    fn start_stream(&mut self, session_id: SessionId) {
        if let Some(task) = self.stream_task.take() {
            task.abort();
        }

        let client = self.client.clone();
        let tx = self.event_tx.clone();
        self.stream_task = Some(tokio::spawn(async move {
            loop {
                let result = client.stream_session_events(session_id, tx.clone()).await;
                let message = match result {
                    Ok(()) => "事件流已关闭，正在重连".to_string(),
                    Err(error) => format!("事件流断开: {error}"),
                };
                if tx
                    .send(NetworkEvent::Disconnected {
                        session_id,
                        error: message,
                    })
                    .is_err()
                {
                    break;
                }
                sleep(Duration::from_secs(1)).await;
            }
        }));
    }

    async fn submit_input(&mut self) -> Result<()> {
        let content = self.input.trim().to_string();
        if content.is_empty() {
            return Ok(());
        }

        let session_id = self.ensure_selected_session(&content).await?;
        self.transcript.push(TranscriptEntry::user(content.clone()));
        self.input.clear();
        self.status = "消息已发送，等待回复...".into();

        match self.client.submit_message(session_id, content).await {
            Ok(_) => {
                self.refresh_requested = true;
            }
            Err(error) => {
                self.transcript
                    .push(TranscriptEntry::error(format!("发送失败: {error}")));
                self.status = format!("发送失败: {error}");
            }
        }

        Ok(())
    }

    fn handle_network_event(&mut self, event: NetworkEvent) {
        match event {
            NetworkEvent::Connected { session_id } => {
                if Some(session_id) == self.selected_session {
                    self.status = format!("已连接事件流: {session_id}");
                }
            }
            NetworkEvent::Disconnected { session_id, error } => {
                if Some(session_id) == self.selected_session {
                    self.status = format!("{error}，1 秒后自动重连。");
                    self.refresh_requested = true;
                }
            }
            NetworkEvent::Assistant(event) => self.apply_assistant_event(event),
        }
    }

    fn apply_assistant_event(&mut self, event: AssistantEvent) {
        match event {
            AssistantEvent::SessionCreated { session, .. } => {
                self.replace_or_insert_session(session);
                self.refresh_requested = true;
            }
            AssistantEvent::MessageAccepted { session_id, .. } => {
                if Some(session_id) == self.selected_session {
                    self.status = "消息已接受，正在生成回复...".into();
                }
            }
            AssistantEvent::ToolStarted {
                session_id,
                tool_name,
                ..
            } => {
                if Some(session_id) == self.selected_session {
                    self.transcript
                        .push(TranscriptEntry::meta(format!("正在执行工具 `{tool_name}`")));
                }
            }
            AssistantEvent::ToolCompleted {
                session_id,
                tool_name,
                output,
                ..
            } => {
                if Some(session_id) == self.selected_session {
                    self.transcript.push(TranscriptEntry::tool(
                        tool_name,
                        output.summary,
                        output.content.clone(),
                    ));
                    self.ignore_completed_content = Some(output.content);
                    self.refresh_requested = true;
                }
            }
            AssistantEvent::MessageChunk {
                session_id,
                content,
                ..
            } => {
                if Some(session_id) == self.selected_session {
                    self.push_assistant_chunk(content);
                }
            }
            AssistantEvent::MessageCompleted {
                session_id,
                content,
                ..
            } => {
                if Some(session_id) == self.selected_session {
                    if self.ignore_completed_content.as_deref() == Some(content.as_str()) {
                        self.ignore_completed_content = None;
                        return;
                    }
                    self.complete_assistant_message(content);
                    self.refresh_requested = true;
                }
            }
            AssistantEvent::Error {
                session_id,
                message,
                ..
            } => {
                if session_id.is_none() || session_id == self.selected_session {
                    self.transcript
                        .push(TranscriptEntry::error(message.clone()));
                    self.status = format!("发生错误: {message}");
                }
            }
        }
    }

    fn push_assistant_chunk(&mut self, content: String) {
        if let Some(last) = self.transcript.last_mut() {
            if last.role == TranscriptRole::Assistant && last.pending {
                last.content.push_str(&content);
                return;
            }
        }

        self.transcript
            .push(TranscriptEntry::assistant(content, true));
    }

    fn complete_assistant_message(&mut self, content: String) {
        if let Some(last) = self.transcript.last_mut() {
            if last.role == TranscriptRole::Assistant && last.pending {
                if last.content != content && !content.is_empty() {
                    last.content = content;
                }
                last.pending = false;
                return;
            }
        }

        self.transcript
            .push(TranscriptEntry::assistant(content, false));
    }

    fn replace_or_insert_session(&mut self, session: SessionSummary) {
        if let Some(existing) = self.sessions.iter_mut().find(|item| item.id == session.id) {
            *existing = session;
        } else {
            self.sessions.insert(0, session);
        }
        self.sessions
            .sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    }

    fn render(&self, frame: &mut Frame) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(8),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(frame.area());

        let body = if frame.area().width >= 90 {
            Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Length(28), Constraint::Min(30)])
                .split(chunks[0])
        } else {
            Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(9), Constraint::Min(20)])
                .split(chunks[0])
        };

        self.render_sessions(frame, body[0]);
        self.render_transcript(frame, body[1]);
        self.render_input(frame, chunks[1]);
        self.render_status(frame, chunks[2]);

        if self.show_help {
            self.render_modal(frame, "帮助", help_text());
        } else if self.show_diagnostics {
            self.render_modal(frame, "诊断", self.diagnostics_text());
        }
    }

    fn render_sessions(&self, frame: &mut Frame, area: Rect) {
        let items = if self.sessions.is_empty() {
            vec![ListItem::new(Line::from("还没有会话"))]
        } else {
            self.sessions
                .iter()
                .map(|session| {
                    let id = short_session_id(session.id);
                    ListItem::new(Text::from(vec![
                        Line::from(Span::styled(
                            session.title.clone(),
                            Style::default().add_modifier(Modifier::BOLD),
                        )),
                        Line::from(Span::styled(id, Style::default().fg(Color::DarkGray))),
                    ]))
                })
                .collect::<Vec<_>>()
        };

        let mut state = ListState::default();
        state.select(self.selected_session.and_then(|selected| {
            self.sessions
                .iter()
                .position(|session| session.id == selected)
        }));

        let title = if self.focus == Focus::Sessions {
            "Sessions [焦点]"
        } else {
            "Sessions"
        };

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(
                Style::default()
                    .bg(Color::Blue)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">");

        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_transcript(&self, frame: &mut Frame, area: Rect) {
        let title = self
            .current_session()
            .map(|session| format!("Conversation - {}", session.title))
            .unwrap_or_else(|| "Conversation".into());

        let transcript = if self.transcript.is_empty() {
            Text::from(vec![
                Line::from("没有消息。"),
                Line::from("按 `n` 创建新会话，或者在输入框直接输入消息后回车。"),
            ])
        } else {
            transcript_text(&self.transcript)
        };

        let width = area.width.saturating_sub(2).max(1);
        let line_count = wrapped_line_count(&transcript, width);
        let block = Block::default().borders(Borders::ALL).title(title);
        let paragraph = Paragraph::new(transcript)
            .block(block)
            .wrap(Wrap { trim: false });
        let visible = area.height.saturating_sub(2) as usize;
        let scroll = line_count.saturating_sub(visible) as u16;
        frame.render_widget(paragraph.scroll((scroll, 0)), area);
    }

    fn render_input(&self, frame: &mut Frame, area: Rect) {
        let title = if self.focus == Focus::Input {
            "Input [焦点]"
        } else {
            "Input"
        };
        let paragraph = Paragraph::new(self.input.as_str())
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false });
        frame.render_widget(paragraph, area);

        if self.focus == Focus::Input {
            let x = area.x + 1 + self.input.chars().count() as u16;
            let max_x = area.x + area.width.saturating_sub(2);
            frame.set_cursor_position(Position::new(cmp::min(x, max_x), area.y + 1));
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let connection = self
            .health
            .as_ref()
            .map(|health| {
                format!(
                    "provider: {} | sessions: {}",
                    health.provider, health.session_count
                )
            })
            .unwrap_or_else(|| "offline".into());
        let focus = match self.focus {
            Focus::Sessions => "focus: sessions",
            Focus::Input => "focus: input",
        };
        let text = format!(
            "{} | {} | {} | `Tab` 切换焦点 `n` 新建 `r` 重连 `d` 诊断 `?` 帮助 `q` 退出",
            self.status, connection, focus
        );
        frame.render_widget(Paragraph::new(text), area);
    }

    fn render_modal(&self, frame: &mut Frame, title: &str, body: Text<'static>) {
        let area = centered_rect(frame.area(), 70, 70);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(body)
                .block(Block::default().borders(Borders::ALL).title(title))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn diagnostics_text(&self) -> Text<'static> {
        let current = self
            .current_session()
            .map(|session| format!("{} ({})", session.title, short_session_id(session.id)))
            .unwrap_or_else(|| "none".into());

        let lines = vec![
            Line::from(format!("gateway: http://{}", self.bind)),
            Line::from(format!(
                "health: {}",
                self.health
                    .as_ref()
                    .map(|health| if health.healthy {
                        "healthy"
                    } else {
                        "degraded"
                    })
                    .unwrap_or("offline")
            )),
            Line::from(format!(
                "provider: {}",
                self.health
                    .as_ref()
                    .map(|health| health.provider.clone())
                    .unwrap_or_else(|| "unknown".into())
            )),
            Line::from(format!("session count: {}", self.sessions.len())),
            Line::from(format!("current session: {current}")),
            Line::from(""),
            Line::from("如果 gateway 没连上："),
            Line::from("1. 先运行 `goldclaw init` 完成初始化"),
            Line::from("2. 如果后台服务没起来，运行 `goldclaw start`"),
            Line::from("3. 回到 TUI 按 `r` 重试连接"),
            Line::from(""),
            Line::from("按 `Esc` 关闭该面板。"),
        ];
        Text::from(lines)
    }

    fn current_session(&self) -> Option<&SessionSummary> {
        let selected = self.selected_session?;
        self.sessions.iter().find(|session| session.id == selected)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct TranscriptEntry {
    role: TranscriptRole,
    content: String,
    pending: bool,
}

impl TranscriptEntry {
    fn user(content: String) -> Self {
        Self {
            role: TranscriptRole::User,
            content,
            pending: false,
        }
    }

    fn assistant(content: String, pending: bool) -> Self {
        Self {
            role: TranscriptRole::Assistant,
            content,
            pending,
        }
    }

    fn tool(tool_name: String, summary: String, content: String) -> Self {
        let body = if summary.is_empty() {
            content
        } else {
            format!("[{tool_name}] {summary}\n{content}")
        };
        Self {
            role: TranscriptRole::Tool,
            content: body,
            pending: false,
        }
    }

    fn error(content: String) -> Self {
        Self {
            role: TranscriptRole::Error,
            content,
            pending: false,
        }
    }

    fn meta(content: String) -> Self {
        Self {
            role: TranscriptRole::Meta,
            content,
            pending: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TranscriptRole {
    User,
    Assistant,
    Tool,
    Error,
    Meta,
}

fn transcript_from_message(message: &SessionMessage) -> TranscriptEntry {
    match message.role {
        MessageRole::User => TranscriptEntry::user(message.content.clone()),
        MessageRole::Assistant => TranscriptEntry::assistant(message.content.clone(), false),
        MessageRole::Tool => {
            let tool_name = message
                .metadata
                .get("tool_name")
                .and_then(|value| value.as_str())
                .unwrap_or("tool")
                .to_string();
            let summary = message
                .metadata
                .get("summary")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .to_string();
            TranscriptEntry::tool(tool_name, summary, message.content.clone())
        }
        MessageRole::System => TranscriptEntry::meta(message.content.clone()),
    }
}

fn transcript_text(entries: &[TranscriptEntry]) -> Text<'static> {
    let mut lines = Vec::new();

    for entry in entries {
        let (label, color) = match entry.role {
            TranscriptRole::User => ("You", Color::Cyan),
            TranscriptRole::Assistant => ("GoldClaw", Color::Green),
            TranscriptRole::Tool => ("Tool", Color::Yellow),
            TranscriptRole::Error => ("Error", Color::Red),
            TranscriptRole::Meta => ("Info", Color::Blue),
        };
        let suffix = if entry.pending { " …" } else { "" };
        lines.push(Line::from(Span::styled(
            format!("{label}{suffix}"),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));

        if entry.content.is_empty() {
            lines.push(Line::from(""));
        } else {
            for raw_line in entry.content.lines() {
                lines.push(Line::from(raw_line.to_string()));
            }
        }
        lines.push(Line::from(""));
    }

    Text::from(lines)
}

fn help_text() -> Text<'static> {
    Text::from(vec![
        Line::from("GoldClaw TUI"),
        Line::from(""),
        Line::from("`Enter` 发送消息"),
        Line::from("`Tab` 在会话列表和输入框之间切换焦点"),
        Line::from("`Up` / `Down` 在左侧切换会话"),
        Line::from("`n` 新建会话"),
        Line::from("`r` 重新拉取状态并重连当前会话事件流"),
        Line::from("`d` 打开诊断面板"),
        Line::from("`Esc` 关闭面板或清空当前输入"),
        Line::from("`q` 或 `Ctrl+C` 退出"),
        Line::from(""),
        Line::from("会话切换后会重新加载历史消息，并自动重连 SSE 事件流。"),
        Line::from("按 `Esc` 关闭该面板。"),
    ])
}

fn centered_rect(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn short_session_id(session_id: SessionId) -> String {
    session_id.to_string().chars().take(8).collect()
}

fn summarize_title(message: &str) -> String {
    let line = message
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("New session")
        .trim();
    let mut title = String::new();
    for ch in line.chars().take(24) {
        title.push(ch);
    }
    if line.chars().count() > 24 {
        title.push_str("...");
    }
    title
}

fn wrapped_line_count(text: &Text<'_>, width: u16) -> usize {
    let width = width.max(1) as usize;
    text.lines
        .iter()
        .map(|line| {
            let line_width = line.width();
            if line_width == 0 {
                1
            } else {
                line_width.div_ceil(width)
            }
        })
        .sum()
}
