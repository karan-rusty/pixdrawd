// SSH front-end: one shared canvas, many clients.
//
// Architecture:
//   * `Shared` owns the authoritative canvas (points + text overlay) and the
//     registry of connected clients.
//   * Each connection spawns a per-client task that owns its own
//     `DrawingApp` (a local replica), its own ratatui `Terminal` writing into
//     the SSH channel, and the channels feeding input/resize/presence.
//   * After handling a batch of local input the client diffs its replica
//     against the authoritative canvas and broadcasts the delta. Peers apply
//     the delta to their replica. Per-cell last-writer-wins.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Position, Rect};
use ratatui::style::Color;
use ratatui::{TerminalOptions, Viewport};
use russh::keys::PrivateKey;
use russh::keys::ssh_key::{Algorithm, LineEnding};
use russh::server::{Auth, Config, Handle, Handler, Msg, Server, Session};
use russh::{Channel, ChannelId, Pty};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::MissedTickBehavior;

use crate::app::{DrawingApp, PeerCursor};

type SshBackend = CrosstermBackend<TerminalHandle>;
type SshTerminal = Terminal<SshBackend>;

/// Enter the alternate screen, enable any-motion (1003) + SGR (1006) mouse
/// reporting, hide the hardware cursor, clear.
const SETUP: &[u8] = b"\x1b[?1049h\x1b[?1003h\x1b[?1006h\x1b[?25l\x1b[2J\x1b[H";
/// Undo everything SETUP did.
const TEARDOWN: &[u8] = b"\x1b[?1003l\x1b[?1006l\x1b[?1049l\x1b[?25h";

/// Fixed colours assigned to clients by id, so everyone sees the same colour
/// for the same person.
const PEER_COLORS: [Color; 8] = [
    Color::Rgb(255, 121, 198),
    Color::Rgb(80, 250, 123),
    Color::Rgb(255, 184, 108),
    Color::Rgb(139, 233, 253),
    Color::Rgb(189, 147, 249),
    Color::Rgb(241, 250, 140),
    Color::Rgb(255, 85, 85),
    Color::Rgb(68, 189, 255),
];

pub fn peer_color(id: usize) -> Color {
    PEER_COLORS[id % PEER_COLORS.len()]
}

#[derive(Clone, Copy)]
pub struct CellOp {
    pub key: (u16, u16),
    /// `Some` paints the cell, `None` erases it.
    pub color: Option<Color>,
}

#[derive(Clone)]
enum Update {
    Cells(Vec<CellOp>),
    Text(Vec<(Position, String)>),
    Cursor {
        id: usize,
        name: String,
        color: Color,
        pos: Option<Position>,
    },
    Bye {
        id: usize,
    },
}

struct Canvas {
    points: BTreeMap<(u16, u16), Color>,
    text_entries: Vec<(Position, String)>,
}

struct ClientEntry {
    name: String,
    color: Color,
    pos: Option<Position>,
    tx: UnboundedSender<Update>,
}

struct Shared {
    canvas: Mutex<Canvas>,
    clients: Mutex<HashMap<usize, ClientEntry>>,
    next_id: AtomicUsize,
}

impl Shared {
    fn new() -> Self {
        Self {
            canvas: Mutex::new(Canvas {
                points: BTreeMap::new(),
                text_entries: Vec::new(),
            }),
            clients: Mutex::new(HashMap::new()),
            next_id: AtomicUsize::new(1),
        }
    }

    fn broadcast(&self, except: usize, update: Update) {
        let clients = self.clients.lock().unwrap();
        for (&id, entry) in clients.iter() {
            if id == except {
                continue;
            }
            let _ = entry.tx.send(update.clone());
        }
    }

    fn peer_list(&self, except: usize) -> Vec<PeerCursor> {
        self.clients
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| *entry.0 != except)
            .map(|(&id, e)| PeerCursor {
                id,
                name: e.name.clone(),
                color: e.color,
                pos: e.pos,
            })
            .collect()
    }
}

pub struct Options {
    pub bind: String,
    pub port: u16,
    pub host_key: String,
}

pub async fn run(opts: Options) -> Result<()> {
    let key = load_or_create_host_key(Path::new(&opts.host_key))?;
    let config = Arc::new(Config {
        inactivity_timeout: Some(Duration::from_secs(3600)),
        auth_rejection_time: Duration::from_secs(0),
        auth_rejection_time_initial: Some(Duration::from_secs(0)),
        keys: vec![key],
        nodelay: true,
        ..Default::default()
    });
    let addr = format!("{}:{}", opts.bind, opts.port);
    let mut server = AppServer {
        shared: Arc::new(Shared::new()),
    };
    println!(
        "pixdrawd listening on {addr}  (connect: ssh -p {} <host>)",
        opts.port
    );
    server.run_on_address(config, addr).await?;
    Ok(())
}

/// Load the server host key, generating and persisting one on first run. The
/// key must be stable across restarts, otherwise every client sees the
/// "REMOTE HOST IDENTIFICATION HAS CHANGED!" warning.
fn load_or_create_host_key(path: &Path) -> Result<PrivateKey> {
    if path.exists() {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading host key {}", path.display()))?;
        return PrivateKey::from_openssh(&text)
            .with_context(|| format!("parsing host key {}", path.display()));
    }
    let key =
        PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).context("generating host key")?;
    let pem = key
        .to_openssh(LineEnding::LF)
        .context("encoding host key")?;
    std::fs::write(path, pem.as_bytes())
        .with_context(|| format!("writing host key {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    eprintln!("generated new host key at {}", path.display());
    Ok(key)
}

struct AppServer {
    shared: Arc<Shared>,
}

impl Server for AppServer {
    type Handler = Client;

    fn new_client(&mut self, _peer_addr: Option<SocketAddr>) -> Client {
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (resize_tx, resize_rx) = mpsc::unbounded_channel();
        Client {
            shared: self.shared.clone(),
            name: String::new(),
            input_tx,
            input_rx: Some(input_rx),
            resize_tx,
            resize_rx: Some(resize_rx),
        }
    }
}

/// One SSH connection. Thin: it forwards raw channel bytes and resize events
/// into the per-client task, which owns the editor.
struct Client {
    shared: Arc<Shared>,
    name: String,
    input_tx: UnboundedSender<Vec<u8>>,
    input_rx: Option<UnboundedReceiver<Vec<u8>>>,
    resize_tx: UnboundedSender<(u32, u32)>,
    resize_rx: Option<UnboundedReceiver<(u32, u32)>>,
}

fn sanitize_name(user: &str) -> String {
    let cleaned: String = user
        .chars()
        .filter(|c| c.is_ascii_graphic() && *c != '@')
        .take(16)
        .collect();
    cleaned
}

impl Handler for Client {
    type Error = anyhow::Error;

    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        self.name = sanitize_name(user);
        Ok(Auth::Accept)
    }

    async fn auth_password(&mut self, user: &str, _password: &str) -> Result<Auth, Self::Error> {
        self.name = sanitize_name(user);
        Ok(Auth::Accept)
    }

    async fn auth_publickey(
        &mut self,
        user: &str,
        _public_key: &russh::keys::PublicKey,
    ) -> Result<Auth, Self::Error> {
        self.name = sanitize_name(user);
        Ok(Auth::Accept)
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let (Some(input_rx), Some(resize_rx)) = (self.input_rx.take(), self.resize_rx.take())
        else {
            // A second session channel on the same connection: refuse.
            return Ok(false);
        };

        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let name = if self.name.is_empty() {
            format!("guest{id}")
        } else {
            self.name.clone()
        };
        let color = peer_color(id);
        let handle = session.handle();
        let channel_id = channel.id();

        let (update_tx, update_rx) = mpsc::unbounded_channel();
        self.shared.clients.lock().unwrap().insert(
            id,
            ClientEntry {
                name: name.clone(),
                color,
                pos: None,
                tx: update_tx,
            },
        );

        let mut app = DrawingApp::new();
        app.username = name.clone();
        {
            let canvas = self.shared.canvas.lock().unwrap();
            app.points = canvas.points.clone();
            app.text_entries = canvas.text_entries.clone();
        }
        app.peers = self.shared.peer_list(id);

        let shared = self.shared.clone();
        tokio::spawn(async move {
            run_client(
                shared.clone(),
                id,
                name,
                color,
                app,
                handle,
                channel_id,
                input_rx,
                resize_rx,
                update_rx,
            )
            .await;
            shared.clients.lock().unwrap().remove(&id);
            shared.broadcast(id, Update::Bye { id });
        });

        Ok(true)
    }

    async fn data(
        &mut self,
        _channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = self.input_tx.send(data.to_vec());
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = self.resize_tx.send((col_width, row_height));
        session.channel_success(channel)?;
        Ok(())
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let _ = self.resize_tx.send((col_width, row_height));
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        Ok(())
    }
}

/// Bridge ratatui's `Write` calls to `Handle::data` on a single writer task,
/// so the terminal state is emitted in order.
struct TerminalHandle {
    sender: UnboundedSender<Vec<u8>>,
    sink: Vec<u8>,
}

impl TerminalHandle {
    fn start(handle: Handle, channel_id: ChannelId) -> Self {
        let (sender, mut receiver) = mpsc::unbounded_channel::<Vec<u8>>();
        tokio::spawn(async move {
            while let Some(data) = receiver.recv().await {
                if handle.data(channel_id, data).await.is_err() {
                    break;
                }
            }
        });
        Self {
            sender,
            sink: Vec::new(),
        }
    }
}

impl std::io::Write for TerminalHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.sink.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.sink.is_empty() {
            return Ok(());
        }
        self.sender
            .send(std::mem::take(&mut self.sink))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "ssh channel closed"))
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_client(
    shared: Arc<Shared>,
    id: usize,
    name: String,
    color: Color,
    mut app: DrawingApp,
    handle: Handle,
    channel: ChannelId,
    mut input_rx: UnboundedReceiver<Vec<u8>>,
    mut resize_rx: UnboundedReceiver<(u32, u32)>,
    mut update_rx: UnboundedReceiver<Update>,
) {
    let backend = CrosstermBackend::new(TerminalHandle::start(handle.clone(), channel));
    let options = TerminalOptions {
        viewport: Viewport::Fixed(Rect::new(0, 0, 80, 24)),
    };
    let Ok(mut terminal) = Terminal::with_options(backend, options) else {
        return;
    };
    let _ = write_bytes(&mut terminal, SETUP);

    // Announce presence so peers add us, and let peers know our cursor.
    shared.broadcast(
        id,
        Update::Cursor {
            id,
            name: name.clone(),
            color,
            pos: None,
        },
    );

    // Redraw and publish at a fixed cadence instead of per event: a mouse drag
    // produces a torrent of tiny updates, and redrawing + re-broadcasting on
    // each one is what made it feel laggy. Coalesce to ~30 fps.
    let mut frame_tick = tokio::time::interval(Duration::from_millis(33));
    frame_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Game of life advances far slower than the frame rate.
    let mut life_tick = tokio::time::interval(Duration::from_millis(150));
    life_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    let mut pending: Vec<u8> = Vec::new();
    let mut pending_ops: Vec<CellOp> = Vec::new();
    let mut pending_text: Option<Vec<(Position, String)>> = None;
    let mut last_cursor: Option<Position> = None;
    let mut needs_sync = false;
    let mut dirty = true;
    // Don't paint the placeholder 80x24 frame before the real pty size lands;
    // if no size ever arrives, fall back after a moment.
    let mut got_size = false;
    let started = std::time::Instant::now();

    loop {
        tokio::select! {
            biased;

            msg = input_rx.recv() => {
                let Some(bytes) = msg else { break };
                pending.extend_from_slice(&bytes);
                if drain_input(&mut pending, &mut app) {
                    needs_sync = true;
                    dirty = true;
                }
                if app.should_quit {
                    break;
                }
            }

            Some((w, h)) = resize_rx.recv() => {
                // A pty with no real terminal behind it reports 0x0; fall back
                // to a sane default rather than a degenerate 1x1 viewport.
                let (w, h) = if w == 0 || h == 0 {
                    (80, 24)
                } else {
                    (w.clamp(1, 1000), h.clamp(1, 1000))
                };
                let _ = terminal.resize(Rect::new(0, 0, w as u16, h as u16));
                // A `Fixed` viewport only repaints changed cells, so when the
                // client's window grows or shrinks the previous frame is left
                // on screen (a "screen inside a screen"). Clear to force a full
                // repaint of the new size.
                let _ = terminal.clear();
                got_size = true;
                dirty = true;
            }

            Some(update) = update_rx.recv() => {
                apply_update(&mut app, update);
                dirty = true;
            }

            _ = life_tick.tick() => {
                if app.life_mode {
                    app.tick();
                    needs_sync = true;
                    dirty = true;
                }
            }

            _ = frame_tick.tick() => {
                // A lone ESC never resolves via the parser; flush it as Esc.
                if pending == [0x1b] {
                    pending.clear();
                    let _ = app.handle_raw_event(&CtEvent::Key(KeyEvent::new(
                        KeyCode::Esc,
                        KeyModifiers::NONE,
                    )));
                    needs_sync = true;
                    dirty = true;
                }
                // Reconcile with the shared canvas once, then publish one batch.
                if needs_sync {
                    let (ops, text) = sync_canvas(&shared, &mut app);
                    pending_ops.extend(ops);
                    if text.is_some() {
                        pending_text = text;
                    }
                    needs_sync = false;
                }
                if !pending_ops.is_empty() {
                    shared.broadcast(id, Update::Cells(std::mem::take(&mut pending_ops)));
                }
                if let Some(text) = pending_text.take() {
                    shared.broadcast(id, Update::Text(text));
                }
                // Re-broadcast the cursor periodically so late joiners catch up.
                broadcast_cursor(&shared, id, &name, color, &app, &mut last_cursor);

                if dirty && (got_size || started.elapsed() >= Duration::from_millis(500)) {
                    if terminal.draw(|frame| app.render(frame)).is_err() {
                        break;
                    }
                    dirty = false;
                }
            }
        }
    }

    let _ = write_bytes(&mut terminal, TEARDOWN);
    drop(terminal);
    let _ = handle.close(channel).await;
}

/// Parse and handle every complete event currently buffered. Returns whether
/// input was handled (and therefore the canvas may have changed).
fn drain_input(pending: &mut Vec<u8>, app: &mut DrawingApp) -> bool {
    let mut handled = false;
    loop {
        match frame_len(pending) {
            Frame::Len(n) => {
                let event = terminput::Event::parse_from(&pending[..n])
                    .ok()
                    .flatten()
                    .and_then(|event| terminput_crossterm::to_crossterm(event).ok());
                if let Some(event) = event {
                    if app.handle_raw_event(&event).is_err() {
                        // ignore individual handler errors
                    }
                    handled = true;
                }
                pending.drain(..n);
            }
            Frame::Incomplete => break,
            // A byte we cannot place (e.g. a terminal response we don't model):
            // drop it and resynchronise.
            Frame::Invalid => {
                pending.remove(0);
            }
        }
        if pending.is_empty() {
            break;
        }
    }
    handled
}

fn apply_update(app: &mut DrawingApp, update: Update) {
    match update {
        Update::Cells(ops) => {
            for op in ops {
                match op.color {
                    Some(color) => {
                        app.points.insert(op.key, color);
                    }
                    None => {
                        app.points.remove(&op.key);
                    }
                }
            }
        }
        Update::Text(entries) => app.text_entries = entries,
        Update::Cursor {
            id,
            name,
            color,
            pos,
        } => {
            if let Some(peer) = app.peers.iter_mut().find(|p| p.id == id) {
                peer.pos = pos;
            } else {
                app.peers.push(PeerCursor {
                    id,
                    name,
                    color,
                    pos,
                });
            }
        }
        Update::Bye { id } => app.peers.retain(|p| p.id != id),
    }
}

/// Reconcile the local replica with the authoritative canvas, applying the
/// delta to the canvas and returning it so the caller can publish it as one
/// batched update (rather than one message per change).
fn sync_canvas(
    shared: &Shared,
    app: &mut DrawingApp,
) -> (Vec<CellOp>, Option<Vec<(Position, String)>>) {
    let mut ops: Vec<CellOp> = Vec::new();
    let mut text: Option<Vec<(Position, String)>> = None;
    {
        let mut canvas = shared.canvas.lock().unwrap();
        for (&key, &value) in app.points.iter() {
            if canvas.points.get(&key) != Some(&value) {
                ops.push(CellOp {
                    key,
                    color: Some(value),
                });
            }
        }
        for &key in canvas.points.keys() {
            if !app.points.contains_key(&key) {
                ops.push(CellOp { key, color: None });
            }
        }
        for op in &ops {
            match op.color {
                Some(value) => {
                    canvas.points.insert(op.key, value);
                }
                None => {
                    canvas.points.remove(&op.key);
                }
            }
        }
        if canvas.text_entries != app.text_entries {
            canvas.text_entries = app.text_entries.clone();
            text = Some(app.text_entries.clone());
        }
    }
    (ops, text)
}

fn broadcast_cursor(
    shared: &Shared,
    id: usize,
    name: &str,
    color: Color,
    app: &DrawingApp,
    last: &mut Option<Position>,
) {
    let pos = app
        .mouse_position
        .and_then(|p| app.local_canvas_position(p));
    if pos == *last {
        return;
    }
    *last = pos;
    if let Some(entry) = shared.clients.lock().unwrap().get_mut(&id) {
        entry.pos = pos;
    }
    shared.broadcast(
        id,
        Update::Cursor {
            id,
            name: name.to_string(),
            color,
            pos,
        },
    );
}

fn write_bytes(terminal: &mut SshTerminal, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let writer = terminal.backend_mut();
    writer.write_all(bytes)?;
    writer.flush()
}

// ---------------------------------------------------------------------------
// Input framing.
//
// `terminput::Event::parse_from` parses one event from the front of a buffer
// but does not report how many bytes it consumed, and some of its branches
// inspect the *last* byte of the buffer. So we must hand it exactly one event.
// This framer mirrors the terminal's byte framing to find that boundary.
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
enum Frame {
    Len(usize),
    Incomplete,
    Invalid,
}

fn frame_len(buf: &[u8]) -> Frame {
    match buf.first() {
        None => Frame::Incomplete,
        Some(0x1b) => frame_esc(buf),
        Some(&b) => match utf8_len(b) {
            0 => Frame::Invalid,
            n if buf.len() < n => Frame::Incomplete,
            n => Frame::Len(n),
        },
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 0,
    }
}

fn frame_esc(buf: &[u8]) -> Frame {
    // A lone ESC is ambiguous: it is either the Esc key or the start of a
    // sequence. Callers resolve it via a timeout; here it stays incomplete.
    if buf.len() == 1 {
        return Frame::Incomplete;
    }
    match buf[1] {
        b'[' => frame_csi(buf),
        b'O' if buf.len() < 3 => Frame::Incomplete,
        b'O' => Frame::Len(3),
        // ESC ESC is Alt+Esc.
        0x1b => Frame::Len(2),
        // ESC <key> is Alt+<key>.
        _ => match frame_len(&buf[1..]) {
            Frame::Len(n) => Frame::Len(1 + n),
            other => other,
        },
    }
}

fn frame_csi(buf: &[u8]) -> Frame {
    if buf.len() < 3 {
        return Frame::Incomplete;
    }
    if buf.starts_with(b"\x1b[200~") {
        // Bracketed paste: runs until the ESC [ 2 0 1 ~ terminator.
        return match find_subslice(&buf[6..], b"\x1b[201~") {
            Some(i) => Frame::Len(6 + i + 6),
            None => Frame::Incomplete,
        };
    }
    // A CSI sequence ends at the first byte in 0x40..=0x7E after "ESC [".
    for (i, &b) in buf.iter().enumerate().skip(2) {
        if (0x40..=0x7e).contains(&b) {
            return Frame::Len(i + 1);
        }
    }
    Frame::Incomplete
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bytes: &[u8]) -> usize {
        match frame_len(bytes) {
            Frame::Len(n) => n,
            other => panic!("expected a complete frame for {bytes:?}, got {other:?}"),
        }
    }

    #[test]
    fn plain_ascii_and_utf8() {
        assert_eq!(frame(b"a"), 1);
        assert_eq!(frame(b"stay"), 1);
        assert_eq!(frame("é".as_bytes()), 2);
        assert_eq!(frame("✔".as_bytes()), 3);
        assert_eq!(frame("😀".as_bytes()), 4);
        assert_eq!(frame_len(&[0xe2]), Frame::Incomplete);
    }

    #[test]
    fn control_keys() {
        assert_eq!(frame(b"\r"), 1);
        assert_eq!(frame(b"\t"), 1);
        assert_eq!(frame(b"\x7f"), 1);
    }

    #[test]
    fn arrows_and_special() {
        assert_eq!(frame(b"\x1b[A"), 3);
        assert_eq!(frame(b"\x1bOA"), 3);
        assert_eq!(frame(b"\x1b[1;5C"), 6);
        assert_eq!(frame(b"\x1b[3~"), 4);
        assert_eq!(frame(b"\x1b[Z"), 3);
    }

    #[test]
    fn mouse_sgr() {
        assert_eq!(frame(b"\x1b[<35;10;5M"), 11);
        assert_eq!(frame(b"\x1b[<35;10;5m"), 11);
    }

    #[test]
    fn alt_key_and_lone_esc() {
        assert_eq!(frame(b"\x1bx"), 2);
        assert_eq!(frame_len(b"\x1b"), Frame::Incomplete);
        assert_eq!(frame(b"\x1b\x1b"), 2);
    }

    #[test]
    fn incomplete_sequences_wait() {
        assert_eq!(frame_len(b"\x1b["), Frame::Incomplete);
        assert_eq!(frame_len(b"\x1b[1;"), Frame::Incomplete);
        assert_eq!(frame_len(b"\x1bO"), Frame::Incomplete);
    }

    #[test]
    fn two_events_in_one_buffer() {
        let buf = b"\x1b[Ax";
        assert_eq!(frame_len(buf), Frame::Len(3));
        assert_eq!(frame_len(&buf[3..]), Frame::Len(1));
    }

    #[test]
    fn bracketed_paste() {
        let buf = b"\x1b[200~hi\x1b[201~";
        assert_eq!(frame_len(buf), Frame::Len(buf.len()));
        assert_eq!(frame_len(b"\x1b[200~hi"), Frame::Incomplete);
    }

    fn test_app() -> DrawingApp {
        let mut app = DrawingApp::new();
        // Inner canvas rect as produced by render_canvas at 80x24.
        app.canvas_area = Rect::new(1, 4, 78, 17);
        app
    }

    #[test]
    fn mouse_click_draws_a_pixel() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = test_app();
        let event = CtEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 40,
            row: 10,
            modifiers: KeyModifiers::NONE,
        });
        app.handle_raw_event(&event).unwrap();
        assert!(
            app.points.contains_key(&(39, 6)),
            "click at screen (40,10) should paint canvas cell (39,6)"
        );
    }

    #[test]
    fn sgr_mouse_bytes_draw_a_pixel() {
        let mut app = test_app();
        let mut pending = b"\x1b[<0;41;11M".to_vec();
        assert!(drain_input(&mut pending, &mut app));
        assert!(
            app.points.contains_key(&(39, 6)),
            "SGR mouse press should parse and paint"
        );
        assert!(pending.is_empty(), "the whole sequence should be consumed");
    }
}
