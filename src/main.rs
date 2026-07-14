use teloxide::{prelude::*, types::{InputFile, InputMedia, InputMediaPhoto}};
use serde::{Deserialize, Serialize};
use log::{error, info, warn, trace, Level, LevelFilter, Metadata, Record};
use chrono::Local;
use std::{fs, path::{Path, PathBuf}, collections::{HashMap, BTreeMap}, thread};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{signal, sync::{mpsc, Mutex, Notify}, time};

/// Safely truncate a string to at most `max_chars` characters,
/// never splitting a multi-byte UTF-8 character.
fn truncate_str(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => &s[..byte_idx],
        None => s,
    }
}

mod config {
    use super::*;

    /// Application configuration loaded from TOML
    #[derive(Deserialize, Debug, Clone)]
    pub struct AppConfig {
        pub telegram: TelegramSettings,
    }

    /// Telegram-specific settings
    #[derive(Deserialize, Debug, Clone)]
    pub struct TelegramSettings {
        pub bot_token: String,
        pub owner_chat_id: i64,
        #[serde(default)]
        pub subscriber_lists: HashMap<String, Vec<i64>>,
        #[serde(default = "default_zmq_endpoint")]
        pub zmq_endpoint: String,
        #[serde(default)]
        pub delivery_receipts_path: Option<PathBuf>,
    }

    /// Default ZMQ endpoint if none specified
    fn default_zmq_endpoint() -> String {
        "tcp://127.0.0.1:6565".to_string()
    }

    impl AppConfig {
        /// Load configuration from ~/.corky/config.toml
        pub fn load() -> Result<Self, String> {
            let home = dirs::home_dir()
                .ok_or_else(|| "Unable to determine home directory".to_string())?;
            let config_path = home.join(".corky").join("config.toml");
            let contents = fs::read_to_string(&config_path)
                .map_err(|e| format!("Failed to read {}: {}", config_path.display(), e))?;
            toml::from_str(&contents)
                .map_err(|e| format!("Failed to parse config TOML: {}", e))
        }
    }
}

mod commands {
    use super::*;
    use teloxide::utils::command::BotCommands;

    /// Supported bot commands
    #[derive(BotCommands, Clone, Debug)]
    #[command(rename_rule = "lowercase", description = "These commands are supported:")]
    pub enum Command {
        #[command(description = "Display this chat's ID.")]
        Id,
        #[command(description = "Show this help text.")]
        Help,
    }

    /// Handle incoming Telegram commands
    pub async fn handle(bot: Bot, msg: Message, cmd: Command) -> ResponseResult<()> {
        let (display_name, username, user_id) = extract_user_info(&msg);
        let response = match cmd {
            Command::Id => {
                let chat_id = msg.chat.id;
                bot.send_message(chat_id, chat_id.to_string()).await?;
                format!("Chat ID: {}", chat_id)
            }
            Command::Help => {
                let help_text = Command::descriptions().to_string();
                bot.send_message(msg.chat.id, help_text.clone()).await?;
                format!("Help: {}", help_text)
            }
        };

        info!(
            "{} | User {} (@{}) id={} invoked {:?}, responded with: {}",
            Local::now().format("%Y-%m-%d %H:%M:%S"),
            display_name,
            username,
            user_id,
            cmd,
            response
        );

        Ok(())
    }

    /// Extract user display name, username, and ID from a Message
    fn extract_user_info(msg: &Message) -> (String, String, String) {
        if let Some(user) = &msg.from {
            let name = user.first_name.clone();
            let uname = user.username.clone().unwrap_or_else(|| "unknown".into());
            let uid = user.id.to_string();
            (name, uname, uid)
        } else {
            ("unknown".into(), "unknown".into(), "unknown".into())
        }
    }
}

#[derive(Deserialize, Debug)]
struct ZmqMessage {
    #[serde(default)]
    chat_id: Option<i64>,
    #[serde(default)]
    subscriber_list: Option<String>,
    text: String,
    #[serde(default)]
    image_path: Option<String>,
    /// Stable identity for acknowledged P8 operations delivery. Existing
    /// senders omit this field and retain the fire-and-forget behavior.
    #[serde(default)]
    delivery_id: Option<String>,
    /// Telegram album fields (Option B). When all three are present and
    /// media_group_size is in 2..=10, this message is batched with siblings
    /// sharing the same media_group_id into a single sendMediaGroup call.
    /// Missing or out-of-range values fall back to single-photo delivery.
    #[serde(default)]
    media_group_id: Option<String>,
    #[serde(default)]
    media_group_size: Option<u32>,
    #[serde(default)]
    media_group_index: Option<u32>,
}

/// Events sent to the central channel
enum Event {
    Zmq(Vec<Vec<u8>>),
}

#[derive(Clone, Debug)]
struct OutboundReply {
    target: Vec<u8>,
    payload: Vec<u8>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DeliveryReceipt {
    delivery_id: String,
    chat_id: i64,
    text: String,
    delivered_at_ms: i64,
}

struct DeliveryReceiptStore {
    path: PathBuf,
    file: File,
    delivered: HashMap<String, DeliveryReceipt>,
}

impl DeliveryReceiptStore {
    fn open(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("create delivery receipt directory: {error}"))?;
        }
        recover_delivery_receipt_tail(&path)?;
        let mut delivered = HashMap::new();
        if path.exists() {
            let file = File::open(&path)
                .map_err(|error| format!("open {}: {error}", path.display()))?;
            for (index, line) in BufReader::new(file).lines().enumerate() {
                let line = line.map_err(|error| {
                    format!("read {} line {}: {error}", path.display(), index + 1)
                })?;
                if line.trim().is_empty() {
                    continue;
                }
                let receipt: DeliveryReceipt = serde_json::from_str(&line).map_err(|error| {
                    format!("parse {} line {}: {error}", path.display(), index + 1)
                })?;
                delivered.insert(receipt.delivery_id.clone(), receipt);
            }
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|error| format!("open {} for append: {error}", path.display()))?;
        sync_receipt_directory(&path)?;
        Ok(Self { path, file, delivered })
    }

    fn matching(&self, delivery_id: &str, chat_id: i64, text: &str) -> Option<bool> {
        self.delivered
            .get(delivery_id)
            .map(|receipt| receipt.chat_id == chat_id && receipt.text == text)
    }

    fn record(&mut self, receipt: DeliveryReceipt) -> Result<(), String> {
        serde_json::to_writer(&mut self.file, &receipt)
            .map_err(|error| format!("serialize delivery receipt: {error}"))?;
        self.file
            .write_all(b"\n")
            .and_then(|()| self.file.flush())
            .and_then(|()| self.file.sync_data())
            .map_err(|error| format!("sync delivery receipt {}: {error}", self.path.display()))?;
        self.delivered.insert(receipt.delivery_id.clone(), receipt);
        Ok(())
    }
}

#[cfg(unix)]
fn sync_receipt_directory(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync receipt directory {}: {error}", parent.display()))
}

#[cfg(not(unix))]
fn sync_receipt_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn recover_delivery_receipt_tail(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| format!("open {} for receipt recovery: {error}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| format!("read {} for receipt recovery: {error}", path.display()))?;
    if bytes.last().is_some_and(|byte| *byte != b'\n') {
        let retained = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        file.set_len(retained as u64)
            .and_then(|()| file.seek(SeekFrom::Start(retained as u64)).map(|_| ()))
            .and_then(|()| file.sync_data())
            .map_err(|error| format!("recover receipt tail {}: {error}", path.display()))?;
    }
    Ok(())
}

type ReceiptState = Arc<Mutex<DeliveryReceiptStore>>;

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum DeliveryAckStatus {
    Delivered,
    RetryableFailure,
    PermanentFailure,
}

#[derive(Debug, Serialize)]
struct DeliveryAck {
    delivery_id: String,
    status: DeliveryAckStatus,
    detail: Option<String>,
}

fn delivery_receipts_path(settings: &config::TelegramSettings) -> Result<PathBuf, String> {
    if let Some(path) = &settings.delivery_receipts_path {
        return Ok(path.clone());
    }
    let home = dirs::home_dir()
        .ok_or_else(|| "Unable to determine home directory for delivery receipts".to_string())?;
    Ok(home.join(".corky").join("telegram-delivery-receipts.jsonl"))
}

fn current_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

fn delivery_ack_payload(ack: &DeliveryAck) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&serde_json::json!(["ok", "delivery_ack", ack]))
        .map_err(|error| format!("serialize delivery acknowledgement: {error}"))
}

// ─── Telegram album batcher (sendMediaGroup) ────────────────────────────────
//
// When incoming ZmqMessages carry (media_group_id, media_group_size,
// media_group_index), they are batched per (group_id, chat_id) until all
// expected photos have arrived OR a deadline elapses. The complete batch
// is flushed via bot.send_media_group(...) so Telegram delivers it as a
// single album. A periodic sweeper task flushes timed-out batches; one-photo
// timeout flushes degrade to send_photo (Telegram albums require 2..=10).
//
// Telegram album constraints applied:
//   - size must be in 2..=10 to attempt media_group; otherwise legacy send_photo
//   - caption is attached ONLY to the index=0 photo; others are empty
//   - photos sorted by index before building Vec<InputMedia>
//
// Backwards-compat: messages without media_group_id take the existing
// send_to_chat_with_image_retry path unchanged.

const BATCH_DEADLINE_SECS: u64 = 5;
const BATCH_SWEEP_INTERVAL_MS: u64 = 1000;
const TELEGRAM_ALBUM_MIN: u32 = 2;
const TELEGRAM_ALBUM_MAX: u32 = 10;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct BatchKey {
    group_id: String,
    chat_id: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PhotoEntry {
    index: u32,
    image_path: String,
    caption: String,
}

/// Pure outcome of flushing a MediaGroupBuf. Extracted from `flush_batch` so
/// the sort/branching/caption-attachment logic is unit-testable without any
/// teloxide/Telegram API dependency.
#[derive(Debug, PartialEq, Eq)]
enum FlushDecision {
    Empty,
    SinglePhoto { photo: PhotoEntry, chat_id: i64 },
    Album { items: Vec<PhotoEntry>, caption_index: u32, chat_id: i64 },
}

/// Resolve a buffered batch into a FlushDecision. Caption_index is always 0 —
/// callers/InputMedia builder are responsible for attaching the caption only
/// to the entry whose `index == caption_index` (or to NO entry, in the partial
/// case where index=0 never arrived).
fn plan_flush(buf: MediaGroupBuf, chat_id: i64) -> FlushDecision {
    // BTreeMap.into_values() iterates in key (index) order ascending.
    let mut items: Vec<PhotoEntry> = buf.photos.into_values().collect();
    match items.len() {
        0 => FlushDecision::Empty,
        1 => FlushDecision::SinglePhoto { photo: items.remove(0), chat_id },
        _ => FlushDecision::Album { items, caption_index: 0, chat_id },
    }
}

/// Pair each album item with an optional caption. The caption attaches ONLY to
/// the entry whose `entry.index == caption_index` (NOT positional). When the
/// idx=0 entry is missing (partial album), no item receives a caption — the
/// caption is the property of the primary chart and degrades silently when the
/// primary photo is absent.
fn build_album_input(
    items: &[PhotoEntry],
    caption_index: u32,
) -> Vec<(&PhotoEntry, Option<String>)> {
    items
        .iter()
        .map(|p| {
            let cap = if p.index == caption_index && !p.caption.is_empty() {
                Some(p.caption.clone())
            } else {
                None
            };
            (p, cap)
        })
        .collect()
}

#[derive(Debug)]
struct MediaGroupBuf {
    expected_size: u32,
    /// Keyed by media_group_index so duplicate-index re-deliveries overwrite
    /// (per Codex pre-impl finding: blind .len() on a Vec can flush early
    /// with duplicates).
    photos: BTreeMap<u32, PhotoEntry>,
    deadline: Instant,
}

type BatcherState = Arc<Mutex<HashMap<BatchKey, MediaGroupBuf>>>;

fn new_batcher() -> BatcherState {
    Arc::new(Mutex::new(HashMap::new()))
}

/// Add a photo to the batch for (group_id, chat_id). If the batch is now
/// complete (unique indexes == expected_size), remove and return it so the
/// caller can flush. Otherwise leaves it in the map for the sweep task to
/// flush on deadline.
async fn enqueue_or_complete(
    state: &BatcherState,
    group_id: String,
    chat_id: i64,
    expected_size: u32,
    index: u32,
    image_path: String,
    caption: String,
) -> Option<MediaGroupBuf> {
    let key = BatchKey { group_id: group_id.clone(), chat_id };
    let mut map = state.lock().await;
    let buf = map.entry(key.clone()).or_insert_with(|| MediaGroupBuf {
        expected_size,
        photos: BTreeMap::new(),
        deadline: Instant::now() + Duration::from_secs(BATCH_DEADLINE_SECS),
    });
    buf.photos.insert(index, PhotoEntry { index, image_path, caption });
    let collected = buf.photos.len() as u32;
    if collected >= buf.expected_size {
        // Complete — remove and return for immediate flush.
        info!(
            "Batcher: group={} chat={} complete ({}/{}), flushing as album",
            group_id, chat_id, collected, buf.expected_size
        );
        map.remove(&key)
    } else {
        info!(
            "Batcher: group={} chat={} queued idx={} ({}/{})",
            group_id, chat_id, index, collected, buf.expected_size
        );
        None
    }
}

/// Drain expired entries from the batcher and return them for flushing.
async fn drain_expired(state: &BatcherState) -> Vec<(BatchKey, MediaGroupBuf)> {
    let now = Instant::now();
    let mut map = state.lock().await;
    let expired_keys: Vec<BatchKey> = map
        .iter()
        .filter(|(_, buf)| buf.deadline <= now)
        .map(|(k, _)| k.clone())
        .collect();
    let mut out = Vec::with_capacity(expired_keys.len());
    for k in expired_keys {
        if let Some(buf) = map.remove(&k) {
            info!(
                "Batcher: group={} chat={} TIMEOUT ({} photos collected, expected {})",
                k.group_id, k.chat_id, buf.photos.len(), buf.expected_size
            );
            out.push((k, buf));
        }
    }
    out
}

/// Flush a MediaGroupBuf: sendMediaGroup when 2..=10 unique photos, otherwise
/// degrade to single send_photo (or skip if 0 photos). Photos sorted by index
/// ascending; caption attached only to index=0.
async fn flush_batch(bot: &Bot, chat_id: i64, buf: MediaGroupBuf) {
    let chat = ChatId(chat_id);

    match plan_flush(buf, chat_id) {
        FlushDecision::Empty => {
            warn!("Batcher: flush called with 0 photos; nothing to send");
        }
        FlushDecision::SinglePhoto { photo, .. } => {
            info!("Batcher: chat={} single-photo fallback ({})", chat_id, photo.image_path);
            send_to_chat_with_image_retry(bot, chat, &photo.caption, &photo.image_path).await;
        }
        FlushDecision::Album { items, caption_index, .. } => {
            let n = items.len() as u32;
            if n > TELEGRAM_ALBUM_MAX {
                warn!(
                    "Batcher: chat={} batch of {} exceeds Telegram album max {}; sending first {} only",
                    chat_id, n, TELEGRAM_ALBUM_MAX, TELEGRAM_ALBUM_MAX
                );
            }

            let paired = build_album_input(&items, caption_index);
            let mut media: Vec<InputMedia> =
                Vec::with_capacity(paired.len().min(TELEGRAM_ALBUM_MAX as usize));
            for (i, (entry, caption_opt)) in paired.iter().enumerate() {
                if i >= TELEGRAM_ALBUM_MAX as usize {
                    break;
                }
                let p = PathBuf::from(&entry.image_path);
                if !p.exists() {
                    error!("Batcher: image file missing at flush time: {}", entry.image_path);
                    continue;
                }
                let input_file = InputFile::file(p);
                let mut photo = InputMediaPhoto::new(input_file);
                if let Some(cap) = caption_opt {
                    photo = photo.caption(cap.clone());
                }
                media.push(InputMedia::Photo(photo));
            }

            if media.len() < TELEGRAM_ALBUM_MIN as usize {
                if media.len() == 1 {
                    if let Some(entry) = items
                        .iter()
                        .find(|e| PathBuf::from(&e.image_path).exists())
                    {
                        send_to_chat_with_image_retry(
                            bot,
                            chat,
                            &entry.caption,
                            &entry.image_path,
                        )
                        .await;
                    }
                } else {
                    error!(
                        "Batcher: chat={} no valid photos after file existence filtering",
                        chat_id
                    );
                }
                return;
            }

            send_media_group_with_retry(bot, chat, media).await;
        }
    }
}

/// Send an album with the same 3-retry exponential backoff as send_photo.
async fn send_media_group_with_retry(bot: &Bot, chat: ChatId, media: Vec<InputMedia>) {
    const MAX_RETRIES: u8 = 3;
    const BASE_DELAY_MS: u64 = 500;
    let n = media.len();
    for attempt in 0..MAX_RETRIES {
        let m = media.clone();
        match time::timeout(time::Duration::from_secs(60), bot.send_media_group(chat, m)).await {
            Ok(Ok(_)) => {
                info!("Sent media group to {} with {} photos", chat, n);
                return;
            }
            Ok(Err(err)) => {
                if attempt < MAX_RETRIES - 1 {
                    let delay = BASE_DELAY_MS * (2_u64.pow(attempt as u32));
                    warn!(
                        "Failed to send media group to {} (attempt {}/{}): {:?}, retrying in {}ms",
                        chat, attempt + 1, MAX_RETRIES, err, delay
                    );
                    time::sleep(time::Duration::from_millis(delay)).await;
                } else {
                    error!(
                        "Failed to send media group to {} after {} attempts: {:?}",
                        chat, MAX_RETRIES, err
                    );
                }
            }
            Err(_elapsed) => {
                if attempt < MAX_RETRIES - 1 {
                    warn!(
                        "Timeout sending media group to {} (attempt {}/{}), retrying",
                        chat, attempt + 1, MAX_RETRIES
                    );
                } else {
                    error!(
                        "Timeout sending media group to {} after {} attempts",
                        chat, MAX_RETRIES
                    );
                }
            }
        }
    }
}

/// Parse and handle raw ZMQ frames
async fn handle_zmq_frames(
    bot: Bot,
    settings: config::TelegramSettings,
    batcher: BatcherState,
    receipts: ReceiptState,
    replies: std::sync::mpsc::Sender<OutboundReply>,
    frames: Vec<Vec<u8>>,
) {
    if frames.len() < 2 {
        warn!("ZMQ: Unexpected frame count: {}", frames.len());
        return;
    }

    info!("ZMQ: Received message with {} frames", frames.len());

    // Log each frame concisely
    for (i, frame) in frames.iter().enumerate() {
        if i < 2 { // Only log first two frames
            match std::str::from_utf8(frame) {
                Ok(txt) => info!("ZMQ: Frame {}: {}", i, txt),
                Err(_) => {
                    let hex_repr = frame.iter()
                        .map(|b| format!("{:02x}", b))
                        .collect::<Vec<_>>()
                        .join("");
                    info!("ZMQ: Frame {} (hex): {}", i, hex_repr);
                }
            }
        }
    }

    // Payload is in frame[1]
    if let Ok(payload) = std::str::from_utf8(&frames[1]) {
        match serde_json::from_str::<serde_json::Value>(payload) {
            Ok(val) => {
                if let Some(arr) = val.as_array() {
                    if arr.len() >= 3 {
                        match serde_json::from_value::<ZmqMessage>(arr[2].clone()) {
                            Ok(cmd) => {
                                info!("ZMQ: Successfully extracted command: {:?}", cmd);
                                if cmd.delivery_id.is_some() {
                                    let ack = process_acknowledged_message(
                                        &bot,
                                        &settings,
                                        &receipts,
                                        &cmd,
                                    )
                                    .await;
                                    match delivery_ack_payload(&ack) {
                                        Ok(payload) => {
                                            if replies.send(OutboundReply {
                                                target: frames[0].clone(),
                                                payload,
                                            }).is_err() {
                                                error!("ZMQ: reply channel closed before delivery acknowledgement");
                                            }
                                        }
                                        Err(error) => error!("ZMQ: {error}"),
                                    }
                                } else {
                                    process_zmq_message(&bot, &settings, &batcher, cmd).await
                                }
                            },
                            Err(err) => error!("Invalid command structure: {:?}", err),
                        }
                    } else {
                        error!("JSON array too short (needs 3+ elements)");
                    }
                } else {
                    error!("JSON payload is not an array");
                }
            },
            Err(err) => error!("Failed to parse JSON: {:?}", err),
        }
    } else {
        error!("Non-UTF8 payload in message");
    }
}

async fn process_acknowledged_message(
    bot: &Bot,
    settings: &config::TelegramSettings,
    receipts: &ReceiptState,
    cmd: &ZmqMessage,
) -> DeliveryAck {
    let delivery_id = cmd.delivery_id.clone().unwrap_or_default();
    if delivery_id.is_empty() {
        return DeliveryAck {
            delivery_id,
            status: DeliveryAckStatus::PermanentFailure,
            detail: Some("delivery_id must not be empty".to_string()),
        };
    }
    if cmd.image_path.is_some() || cmd.subscriber_list.is_some() {
        return DeliveryAck {
            delivery_id,
            status: DeliveryAckStatus::PermanentFailure,
            detail: Some(
                "acknowledged delivery supports one text chat; images and subscriber lists remain legacy"
                    .to_string(),
            ),
        };
    }
    let chat_id = cmd.chat_id.unwrap_or(settings.owner_chat_id);
    // Serialize each acknowledged identity through the receipt store. This
    // prevents two concurrent redeliveries from both reaching Telegram.
    let mut store = receipts.lock().await;
    match store.matching(&delivery_id, chat_id, &cmd.text) {
        Some(true) => {
            return DeliveryAck {
                delivery_id,
                status: DeliveryAckStatus::Delivered,
                detail: Some("already delivered".to_string()),
            };
        }
        Some(false) => {
            return DeliveryAck {
                delivery_id,
                status: DeliveryAckStatus::PermanentFailure,
                detail: Some("delivery_id reused with different content".to_string()),
            };
        }
        None => {}
    }
    if let Err(error) = send_to_chat_with_retry_result(bot, ChatId(chat_id), &cmd.text).await {
        return DeliveryAck {
            delivery_id,
            status: DeliveryAckStatus::RetryableFailure,
            detail: Some(error),
        };
    }
    let receipt = DeliveryReceipt {
        delivery_id: delivery_id.clone(),
        chat_id,
        text: cmd.text.clone(),
        delivered_at_ms: current_epoch_ms(),
    };
    match store.record(receipt) {
        Ok(()) => DeliveryAck {
            delivery_id,
            status: DeliveryAckStatus::Delivered,
            detail: None,
        },
        Err(error) => DeliveryAck {
            delivery_id,
            status: DeliveryAckStatus::RetryableFailure,
            detail: Some(format!(
                "Telegram accepted message but receipt persistence failed; outcome uncertain: {error}"
            )),
        },
    }
}

/// Dispatch ZMQ command to appropriate chats
async fn process_zmq_message(
    bot: &Bot,
    settings: &config::TelegramSettings,
    batcher: &BatcherState,
    cmd: ZmqMessage,
) {
    info!("Processing ZMQ message: {:?}", cmd);

    // ── Album batching path ──────────────────────────────────────────────────
    // When all three media_group_* fields are present AND size is in the
    // Telegram-supported range 2..=10, route each photo through the batcher.
    // Anything else (missing fields, invalid size) falls through to the
    // existing per-message send_photo path.
    let album_valid = cmd.media_group_id.is_some()
        && cmd.media_group_size
            .map(|s| (TELEGRAM_ALBUM_MIN..=TELEGRAM_ALBUM_MAX).contains(&s))
            .unwrap_or(false)
        && cmd.media_group_index.is_some()
        && cmd.image_path.is_some();
    if album_valid {
        let group_id = cmd.media_group_id.clone().expect("checked");
        let size = cmd.media_group_size.expect("checked");
        let index = cmd.media_group_index.expect("checked");
        let image_path = cmd.image_path.clone().expect("checked");
        let caption = cmd.text.clone();

        // Resolve the chat list (single chat_id OR subscriber_list expansion)
        let chat_targets: Vec<i64> = if let Some(cid) = cmd.chat_id {
            vec![cid]
        } else if let Some(list_name) = cmd.subscriber_list.as_ref() {
            if let Some(subs) = settings.subscriber_lists.get(list_name) {
                subs.iter().copied().collect()
            } else {
                warn!("Subscriber list '{}' not found; using owner_chat_id", list_name);
                vec![settings.owner_chat_id]
            }
        } else {
            vec![settings.owner_chat_id]
        };

        // For each chat, enqueue into its own per-(group_id, chat_id) batch.
        // Completed batches are flushed immediately; partial batches wait for
        // their siblings or the sweep task's deadline check.
        for chat_id in chat_targets {
            let maybe_complete = enqueue_or_complete(
                batcher,
                group_id.clone(),
                chat_id,
                size,
                index,
                image_path.clone(),
                caption.clone(),
            )
            .await;
            if let Some(buf) = maybe_complete {
                flush_batch(bot, chat_id, buf).await;
            }
        }
        return;
    }

    // ── Legacy single-message path (unchanged) ───────────────────────────────
    if let Some(chat_id) = cmd.chat_id {
        if let Some(img_path) = &cmd.image_path {
            send_to_chat_with_image_retry(bot, ChatId(chat_id), &cmd.text, img_path).await;
        } else {
            send_to_chat_with_retry(bot, ChatId(chat_id), &cmd.text).await;
        }
    } else if let Some(list_name) = &cmd.subscriber_list {
        if let Some(subs) = settings.subscriber_lists.get(list_name) {
            let mut tasks = tokio::task::JoinSet::new();
            for &sub_id in subs {
                let bot = bot.clone();
                let text = cmd.text.clone();
                let image_path = cmd.image_path.clone();
                tasks.spawn(async move {
                    if let Some(img_path) = &image_path {
                        send_to_chat_with_image_retry(&bot, ChatId(sub_id), &text, img_path).await;
                    } else {
                        send_to_chat_with_retry(&bot, ChatId(sub_id), &text).await;
                    }
                });
            }
            while tasks.join_next().await.is_some() {}
        } else {
            warn!("Subscriber list '{}' not found", list_name);
            send_to_chat_with_retry(
                bot,
                ChatId(settings.owner_chat_id),
                &format!("Warning: unknown subscriber list '{}'", list_name),
            ).await;
        }
    } else if let Some(img_path) = &cmd.image_path {
        send_to_chat_with_image_retry(bot, ChatId(settings.owner_chat_id), &cmd.text, img_path).await;
    } else {
        send_to_chat_with_retry(bot, ChatId(settings.owner_chat_id), &cmd.text).await;
    }
}

/// Send a message with retry logic for resilience
async fn send_to_chat_with_retry(bot: &Bot, chat: ChatId, text: &str) {
    let _ = send_to_chat_with_retry_result(bot, chat, text).await;
}

async fn send_to_chat_with_retry_result(
    bot: &Bot,
    chat: ChatId,
    text: &str,
) -> Result<(), String> {
    const MAX_RETRIES: u8 = 3;
    const BASE_DELAY_MS: u64 = 500;
    let mut last_error = "Telegram delivery failed".to_string();

    for attempt in 0..MAX_RETRIES {
        match time::timeout(
            time::Duration::from_secs(30),
            bot.send_message(chat, text),
        ).await {
            Ok(Ok(_)) => {
                info!("Sent message to {}: \"{}\"", chat, if text.len() > 30 { format!("{}...", truncate_str(text, 30)) } else { text.to_string() });
                return Ok(());
            }
            Ok(Err(err)) => {
                last_error = format!("Telegram API error: {err}");
                if attempt < MAX_RETRIES - 1 {
                    let delay = BASE_DELAY_MS * (2_u64.pow(attempt as u32));
                    warn!("Failed to send to {} (attempt {}/{}): {:?}, retrying in {}ms",
                          chat, attempt + 1, MAX_RETRIES, err, delay);
                    time::sleep(time::Duration::from_millis(delay)).await;
                } else {
                    error!("Failed to send to {} after {} attempts: {:?}", chat, MAX_RETRIES, err);
                }
            }
            Err(_elapsed) => {
                last_error = "Telegram API timeout".to_string();
                if attempt < MAX_RETRIES - 1 {
                    warn!("Timeout sending to {} (attempt {}/{}), retrying", chat, attempt + 1, MAX_RETRIES);
                } else {
                    error!("Timeout sending to {} after {} attempts", chat, MAX_RETRIES);
                }
            }
        }
    }
    Err(last_error)
}

/// Send a message with an image with retry logic for resilience
async fn send_to_chat_with_image_retry(bot: &Bot, chat: ChatId, text: &str, image_path: &str) {
    const MAX_RETRIES: u8 = 3;
    const BASE_DELAY_MS: u64 = 500;
    
    let path = PathBuf::from(image_path);
    if !path.exists() {
        error!("Image file not found: {}", image_path);
        // Fall back to sending just the text
        send_to_chat_with_retry(bot, chat, text).await;
        return;
    }

    for attempt in 0..MAX_RETRIES {
        let path = PathBuf::from(image_path);
        let input_file = InputFile::file(path);

        match time::timeout(
            time::Duration::from_secs(60),
            bot.send_photo(chat, input_file.clone()).caption(text),
        ).await {
            Ok(Ok(_)) => {
                info!("Sent image message to {}: \"{}\" with image {}",
                      chat,
                      if text.len() > 30 { format!("{}...", truncate_str(text, 30)) } else { text.to_string() },
                      image_path);
                return;
            }
            Ok(Err(err)) => {
                if attempt < MAX_RETRIES - 1 {
                    let delay = BASE_DELAY_MS * (2_u64.pow(attempt as u32));
                    warn!("Failed to send image to {} (attempt {}/{}): {:?}, retrying in {}ms",
                          chat, attempt + 1, MAX_RETRIES, err, delay);
                    time::sleep(time::Duration::from_millis(delay)).await;
                } else {
                    error!("Failed to send image to {} after {} attempts: {:?}", chat, MAX_RETRIES, err);
                    warn!("Falling back to text-only message");
                    send_to_chat_with_retry(bot, chat, &format!("{} (Image attachment failed: {})", text, image_path)).await;
                }
            }
            Err(_elapsed) => {
                if attempt < MAX_RETRIES - 1 {
                    warn!("Timeout sending image to {} (attempt {}/{}), retrying", chat, attempt + 1, MAX_RETRIES);
                } else {
                    error!("Timeout sending image to {} after {} attempts", chat, MAX_RETRIES);
                    warn!("Falling back to text-only message");
                    send_to_chat_with_retry(bot, chat, &format!("{} (Image attachment failed: {})", text, image_path)).await;
                }
            }
        }
    }
}

/// Set up a custom logger with condensed, colorful output
fn setup_logger() {
    struct CustomLogger;

    impl log::Log for CustomLogger {
        fn enabled(&self, metadata: &Metadata) -> bool {
            metadata.level() <= Level::Info
        }

        fn log(&self, record: &Record) {
            if self.enabled(record.metadata()) {
                let timestamp = Local::now().format("%H:%M:%S").to_string();
                let message = record.args().to_string();

                // Color coding based on message type and level
                let (color_code, prefix) = match record.level() {
                    Level::Error => ("\x1b[31m", "ERROR"), // Red for errors
                    Level::Warn => ("\x1b[33m", "WARN "), // Yellow for warnings
                    Level::Info => {
                        if message.contains("ZMQ:") {
                            if message.contains("received message") || message.contains("Received message") {
                                ("\x1b[36m", "ZMQ ") // Cyan for ZMQ received messages
                            } else {
                                ("\x1b[90m", "ZMQ ") // Dark gray for other ZMQ messages
                            }
                        } else if message.contains("telegram") || message.contains("bot") {
                            ("\x1b[32m", "BOT ") // Green for bot-related messages
                        } else if message.contains("Processing") || message.contains("command") {
                            ("\x1b[35m", "CMD ") // Magenta for command processing
                        } else if message.contains("Sent message") {
                            ("\x1b[34m", "MSG ") // Blue for sent messages
                        } else {
                            ("\x1b[0m", "INFO") // Default for other info messages
                        }
                    }
                    _ => ("\x1b[0m", "INFO"), // Default color for other levels
                };

                // Reset color code at the end
                let reset_code = "\x1b[0m";
                let log_message = if message.contains("ZMQ:") {
                    // For ZMQ messages, extract just the important parts
                    if message.contains("poll detected") || message.contains("entering") || 
                       message.contains("poll error") || message.contains("timeout") {
                        // Skip verbose polling messages
                        return;
                    } else if let Some(idx) = message.find("Frame 0:") {
                        // For frame logging, condense to show just the sender
                        format!("From: {}", message.get(idx + 8..).unwrap_or("").trim())
                    } else if message.contains("Frame 1:") && message.contains("send_message") {
                        // For message content, extract key parts to make it more readable
                        let content = message.find("Frame 1:")
                            .and_then(|idx| message.get(idx + 8..))
                            .unwrap_or("")
                            .trim();
                        if content.contains("text") {
                            if let Some(text_start) = content.find("\"text\":") {
                                let text_content = content.get(text_start + 8..).unwrap_or("");
                                if let Some(end) = text_content.find("\",") {
                                    format!("Content: {}", text_content.get(..end).unwrap_or(text_content))
                                } else if let Some(end) = text_content.find("\"}") {
                                    format!("Content: {}", text_content.get(..end).unwrap_or(text_content))
                                } else {
                                    format!("Message: {}", content)
                                }
                            } else {
                                format!("Message: {}", content)
                            }
                        } else {
                            format!("Message: {}", content)
                        }
                    } else if message.contains("Successfully extracted command") {
                        // Extract just the command details
                        if let Some(idx) = message.find("command:") {
                            format!("Command: {}", message.get(idx + 8..).unwrap_or("").trim())
                        } else {
                            message.clone()
                        }
                    } else if message.contains("Processing ZMQ message") {
                        // Extract just the essential parts
                        "Processing message".to_string()
                    } else {
                        // Keep other ZMQ messages as is, but without the prefix
                        message.replace("ZMQ: ", "")
                    }
                } else {
                    message
                };
                
                // Condensed output format: [time] [type] message
                println!("{}{} [{}] {}{}", color_code, timestamp, prefix, log_message, reset_code);
            }
        }

        fn flush(&self) {}
    }

    let _ = log::set_boxed_logger(Box::new(CustomLogger)).map(|()| log::set_max_level(LevelFilter::Info));
}

#[tokio::main]
async fn main() {
    // Initialize custom logger
    setup_logger();
    info!("Starting telegram_zmq_bot…");

    // Load config
    let app_config = match config::AppConfig::load() {
        Ok(cfg) => cfg,
        Err(err) => {
            error!("{}", err);
            error!("Ensure ~/.corky/config.toml exists with a [telegram] section");
            return;
        }
    };
    let settings = app_config.telegram.clone();

    // Create bot
    let bot = Bot::new(&settings.bot_token);
    let receipt_path = match delivery_receipts_path(&settings) {
        Ok(path) => path,
        Err(error) => {
            error!("Cannot configure durable delivery receipts: {error}");
            return;
        }
    };
    let receipts = match DeliveryReceiptStore::open(receipt_path) {
        Ok(store) => Arc::new(Mutex::new(store)),
        Err(error) => {
            error!("Cannot open durable delivery receipts: {error}");
            return;
        }
    };

    // Telegram album batcher: shared across all incoming-message handlers.
    // Photos arriving with the same media_group_id accumulate here until
    // complete or the deadline elapses, then flush as a single sendMediaGroup.
    let batcher: BatcherState = new_batcher();

    // Periodic sweeper: every BATCH_SWEEP_INTERVAL_MS, scan the batcher for
    // entries past their deadline and flush whatever photos arrived. Runs for
    // the lifetime of the process; tokio shutdown is implicit at process exit.
    {
        let bot = bot.clone();
        let batcher = batcher.clone();
        tokio::spawn(async move {
            let mut interval =
                time::interval(time::Duration::from_millis(BATCH_SWEEP_INTERVAL_MS));
            interval.tick().await; // skip the immediate first tick
            loop {
                interval.tick().await;
                let expired = drain_expired(&batcher).await;
                for (key, buf) in expired {
                    flush_batch(&bot, key.chat_id, buf).await;
                }
            }
        });
    }

    // Central event channel (bounded to prevent unbounded memory growth)
    let (tx, mut rx) = mpsc::channel::<Event>(256);
    let (reply_tx, reply_rx) = std::sync::mpsc::channel::<OutboundReply>();

    // Shutdown flag shared with the ZMQ thread
    let shutdown_flag = Arc::new(AtomicBool::new(false));

    // Spawn ZMQ listener in a dedicated thread
    let zmq_handle = {
        let tx = tx.clone();
        let endpoint = settings.zmq_endpoint.clone();
        let shutdown = shutdown_flag.clone();
        thread::spawn(move || {
            info!("ZMQ: Starting listener thread");
            let context = zmq::Context::new();

            // Outer reconnection loop
            while !shutdown.load(Ordering::Acquire) {
                let socket = match context.socket(zmq::DEALER) {
                    Ok(s) => s,
                    Err(e) => {
                        error!("Failed to create ZMQ socket: {:?}, retrying in 5s", e);
                        std::thread::sleep(std::time::Duration::from_secs(5));
                        continue;
                    }
                };

                // Set identity exactly like the Python script
                let identity = b"telegram".to_vec();
                if let Err(e) = socket.set_identity(&identity) {
                    error!("Failed to set ZMQ identity: {:?}, retrying in 5s", e);
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    continue;
                }

                info!("ZMQ: DEALER socket connecting to {}", endpoint);
                match socket.connect(&endpoint) {
                    Ok(_) => info!("ZMQ: Successfully connected to {}", endpoint),
                    Err(e) => {
                        error!("Failed to connect to ZMQ endpoint: {:?}, retrying in 5s", e);
                        std::thread::sleep(std::time::Duration::from_secs(5));
                        continue;
                    }
                }

                // Set socket options for better reliability
                if let Err(e) = socket.set_linger(0) {
                    warn!("Failed to set ZMQ linger option: {:?}", e);
                }

                if let Err(e) = socket.set_reconnect_ivl(1000) {
                    warn!("Failed to set ZMQ reconnect interval: {:?}", e);
                }

                if let Err(e) = socket.set_reconnect_ivl_max(30000) {
                    warn!("Failed to set ZMQ max reconnect interval: {:?}", e);
                }

                // Create items for polling, similar to Python implementation
                let mut items = [socket.as_poll_item(zmq::POLLIN)];
                info!("ZMQ: Entering polling loop");

                // Connection health check tracker
                let mut consecutive_errors = 0;
                let max_consecutive_errors = 10;

                // Inner polling loop - runs until max consecutive errors or shutdown
                while consecutive_errors < max_consecutive_errors && !shutdown.load(Ordering::Acquire) {
                    while let Ok(reply) = reply_rx.try_recv() {
                        match socket.send_multipart(
                            [reply.target.as_slice(), reply.payload.as_slice()],
                            zmq::DONTWAIT,
                        ) {
                            Ok(()) => {}
                            Err(error) => {
                                error!("ZMQ: failed to send delivery acknowledgement: {error}");
                            }
                        }
                    }
                    // Short poll keeps delivery acknowledgements and shutdown responsive.
                    match zmq::poll(&mut items, 100) {
                        Ok(0) => {
                            // No events, just a timeout
                            trace!("ZMQ: Poll timeout, connection still alive");
                        },
                        Ok(_) => {
                            // Check if our socket has data
                            if items[0].get_revents().contains(zmq::POLLIN) {
                                match socket.recv_multipart(0) {
                                    Ok(frames) => {
                                        info!("ZMQ: Received message with {} frames", frames.len());
                                        let mut event = Event::Zmq(frames);
                                        loop {
                                            match tx.try_send(event) {
                                                Ok(()) => break,
                                                Err(mpsc::error::TrySendError::Full(returned)) => {
                                                    if shutdown.load(Ordering::Acquire) {
                                                        info!("ZMQ: Shutdown during channel-full, exiting");
                                                        return;
                                                    }
                                                    event = returned;
                                                    std::thread::sleep(std::time::Duration::from_millis(50));
                                                }
                                                Err(mpsc::error::TrySendError::Closed(_)) => {
                                                    info!("ZMQ: Channel closed, shutting down");
                                                    return;
                                                }
                                            }
                                        }
                                        consecutive_errors = 0;
                                    }
                                    Err(err) => {
                                        error!("ZMQ recv error: {:?}", err);
                                        consecutive_errors += 1;
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            error!("ZMQ poll error: {:?}", err);
                            consecutive_errors += 1;
                        }
                    }
                }

                if shutdown.load(Ordering::Acquire) {
                    break;
                }

                // If we reached max consecutive errors, close socket and reconnect
                error!("ZMQ: Too many consecutive errors ({}), reconnecting...", max_consecutive_errors);
                let _ = socket.disconnect(&endpoint);
                drop(socket);
                std::thread::sleep(std::time::Duration::from_secs(5));
            }

            info!("ZMQ: Listener thread exiting");
        })
    };

    // Shutdown notification for instant signaling
    let shutdown_notify = Arc::new(Notify::new());

    // Spawn CTRL+C handler
    {
        let shutdown_flag = shutdown_flag.clone();
        let shutdown_notify = shutdown_notify.clone();
        tokio::spawn(async move {
            if signal::ctrl_c().await.is_ok() {
                info!("CTRL+C received; initiating shutdown");
                shutdown_flag.store(true, Ordering::Release);
                shutdown_notify.notify_one();
            }
        });
    }

    // Telegram command dispatcher (no internal CTRL+C handler).
    // Two branches: regular messages (private/group chats) and channel posts
    // (broadcast channels). Channel posts arrive on update.channel_post, not
    // update.message, so they need their own filter — but commands::handle
    // accepts either since both are teloxide `Message`s.
    let handler = dptree::entry()
        .branch(
            Update::filter_message()
                .filter_command::<commands::Command>()
                .endpoint(commands::handle),
        )
        .branch(
            Update::filter_channel_post()
                .filter_command::<commands::Command>()
                .endpoint(commands::handle),
        );
    let mut dispatcher = Dispatcher::builder(bot.clone(), handler).build();
    let dispatch_shutdown = dispatcher.shutdown_token();
    let dispatch_task = tokio::spawn(async move {
        dispatcher.dispatch().await;
    });

    // Central event loop: handle ZMQ messages or shutdown via select!
    loop {
        tokio::select! {
            _ = shutdown_notify.notified() => {
                info!("Shutdown signal received; exiting event loop");
                break;
            }
            event = rx.recv() => {
                match event {
                    Some(Event::Zmq(frames)) => {
                        let bot = bot.clone();
                        let settings = settings.clone();
                        let batcher = batcher.clone();
                        let receipts = receipts.clone();
                        let replies = reply_tx.clone();
                        tokio::spawn(async move {
                            handle_zmq_frames(
                                bot,
                                settings,
                                batcher,
                                receipts,
                                replies,
                                frames,
                            ).await;
                        });
                    }
                    None => {
                        info!("Event channel closed; exiting event loop");
                        break;
                    }
                }
            }
        }
    }

    // Shut down the Telegram dispatcher gracefully
    if let Ok(fut) = dispatch_shutdown.shutdown() {
        if time::timeout(time::Duration::from_secs(10), fut).await.is_err() {
            warn!("Telegram dispatcher shutdown timed out");
        }
    }
    dispatch_task.abort();

    // Signal the ZMQ thread to stop and wait for it
    shutdown_flag.store(true, Ordering::Release);
    info!("Waiting for ZMQ thread to exit...");
    if let Err(e) = zmq_handle.join() {
        error!("ZMQ thread panicked: {:?}", e);
    }

    info!("telegram_zmq_bot has shut down gracefully");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_ascii() {
        assert_eq!(truncate_str("hello world", 5), "hello");
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate_str("hi", 10), "hi");
    }

    #[test]
    fn truncate_empty() {
        assert_eq!(truncate_str("", 5), "");
    }

    #[test]
    fn truncate_emoji() {
        // Each emoji is one char but multiple bytes
        let s = "\u{1F600}\u{1F601}\u{1F602}\u{1F603}\u{1F604}"; // 5 emojis
        let result = truncate_str(s, 3);
        assert_eq!(result, "\u{1F600}\u{1F601}\u{1F602}");
    }

    #[test]
    fn truncate_mixed_utf8() {
        let s = "aBC\u{00E9}\u{00E8}fg"; // a B C é è f g
        let result = truncate_str(s, 4);
        assert_eq!(result, "aBC\u{00E9}");
    }

    #[test]
    fn truncate_exact_boundary() {
        assert_eq!(truncate_str("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_zero() {
        assert_eq!(truncate_str("hello", 0), "");
    }

    #[test]
    fn truncate_cjk() {
        let s = "\u{4F60}\u{597D}\u{4E16}\u{754C}"; // 你好世界
        let result = truncate_str(s, 2);
        assert_eq!(result, "\u{4F60}\u{597D}");
    }

    // ── Batcher (G3b) tests ─────────────────────────────────────────────────
    use std::collections::BTreeMap;

    fn mk_photo(index: u32, caption: &str) -> PhotoEntry {
        PhotoEntry {
            index,
            image_path: format!("/tmp/img_{}.png", index),
            caption: caption.to_string(),
        }
    }

    fn mk_buf(expected: u32, deadline: Instant) -> MediaGroupBuf {
        MediaGroupBuf {
            expected_size: expected,
            photos: BTreeMap::new(),
            deadline,
        }
    }

    // 6. Completion semantics: inserting `expected_size` unique indexes via the
    //    raw API mirrors what `enqueue_or_complete` would observe right before
    //    it removes the entry.
    #[test]
    fn enqueue_triggers_completion_when_size_reached() {
        let mut buf = mk_buf(2, Instant::now() + Duration::from_secs(5));
        buf.photos.insert(0, mk_photo(0, "primary"));
        buf.photos.insert(1, mk_photo(1, ""));
        assert_eq!(buf.photos.len() as u32, buf.expected_size);
    }

    // 7. BTreeMap.insert overwrites on duplicate key — preserves
    //    "duplicate-index re-deliveries replace, not append".
    #[test]
    fn duplicate_index_replaces_not_appends() {
        let mut buf = mk_buf(2, Instant::now() + Duration::from_secs(5));
        buf.photos.insert(0, mk_photo(0, "first"));
        buf.photos.insert(0, mk_photo(0, "second"));
        assert_eq!(buf.photos.len(), 1);
        assert_eq!(buf.photos.get(&0).unwrap().caption, "second");
    }

    // 8. drain_expired selects only entries whose deadline <= now.
    #[tokio::test]
    async fn drain_expired_returns_only_past_deadline_keys() {
        let state: BatcherState = new_batcher();
        {
            let mut map = state.lock().await;
            map.insert(
                BatchKey { group_id: "past".to_string(), chat_id: 1 },
                mk_buf(2, Instant::now() - Duration::from_secs(1)),
            );
            map.insert(
                BatchKey { group_id: "future".to_string(), chat_id: 1 },
                mk_buf(2, Instant::now() + Duration::from_secs(30)),
            );
        }
        let drained = drain_expired(&state).await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0.group_id, "past");
        // Map still contains the future entry.
        let map = state.lock().await;
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&BatchKey { group_id: "future".to_string(), chat_id: 1 }));
    }

    // 9. Empty buffer → Empty decision.
    #[test]
    fn plan_flush_empty_returns_empty() {
        let buf = mk_buf(2, Instant::now() + Duration::from_secs(5));
        assert_eq!(plan_flush(buf, 42), FlushDecision::Empty);
    }

    // 10. Complete album: items in index order, caption_index=0, chat_id passthrough.
    #[test]
    fn plan_flush_complete_emits_sorted_album() {
        let mut buf = mk_buf(2, Instant::now() + Duration::from_secs(5));
        // Insert out of order to verify BTreeMap sort.
        buf.photos.insert(1, mk_photo(1, ""));
        buf.photos.insert(0, mk_photo(0, "primary"));
        match plan_flush(buf, 42) {
            FlushDecision::Album { items, caption_index, chat_id } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].index, 0);
                assert_eq!(items[1].index, 1);
                assert_eq!(caption_index, 0);
                assert_eq!(chat_id, 42);
            }
            other => panic!("expected Album, got {:?}", other),
        }
    }

    // 11. Partial album (expected_size=3, only 2 collected) still planned as Album
    //     when len >= 2, in sorted order.
    #[test]
    fn plan_flush_partial_with_multiple_photos_emits_sorted_album() {
        let mut buf = mk_buf(3, Instant::now() + Duration::from_secs(5));
        buf.photos.insert(2, mk_photo(2, ""));
        buf.photos.insert(0, mk_photo(0, "primary"));
        match plan_flush(buf, 7) {
            FlushDecision::Album { items, caption_index, chat_id } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].index, 0);
                assert_eq!(items[1].index, 2);
                assert_eq!(caption_index, 0);
                assert_eq!(chat_id, 7);
            }
            other => panic!("expected Album, got {:?}", other),
        }
    }

    // 12. Single photo → SinglePhoto with chat_id passthrough.
    #[test]
    fn plan_flush_single_photo_returns_single_photo() {
        let mut buf = mk_buf(2, Instant::now() + Duration::from_secs(5));
        buf.photos.insert(0, mk_photo(0, "lonely"));
        match plan_flush(buf, 99) {
            FlushDecision::SinglePhoto { photo, chat_id } => {
                assert_eq!(photo.index, 0);
                assert_eq!(photo.caption, "lonely");
                assert_eq!(chat_id, 99);
            }
            other => panic!("expected SinglePhoto, got {:?}", other),
        }
    }

    // 13. Boundary: deadline == now is considered expired (<= semantics).
    #[tokio::test]
    async fn drain_expired_boundary_exactly_now() {
        let state: BatcherState = new_batcher();
        let now = Instant::now();
        {
            let mut map = state.lock().await;
            map.insert(
                BatchKey { group_id: "exactly-now".to_string(), chat_id: 1 },
                mk_buf(2, now),
            );
        }
        // Ensure now() inside drain_expired is >= the stored deadline.
        tokio::time::sleep(Duration::from_millis(1)).await;
        let drained = drain_expired(&state).await;
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].0.group_id, "exactly-now");
    }

    // 14. Caption attaches ONLY to the entry whose index == caption_index.
    #[test]
    fn plan_flush_caption_only_attached_to_index_zero() {
        let items = vec![mk_photo(0, "primary"), mk_photo(1, "wide")];
        let paired = build_album_input(&items, 0);
        assert_eq!(paired.len(), 2);
        assert_eq!(paired[0].1.as_deref(), Some("primary"));
        assert_eq!(paired[1].1, None);
    }

    // 15. Duplicate index after partial completion: replacement preserves
    //     completion state (len == expected_size, second caption wins).
    #[test]
    fn duplicate_index_completion_interaction() {
        let mut buf = mk_buf(2, Instant::now() + Duration::from_secs(5));
        buf.photos.insert(0, mk_photo(0, "first"));
        buf.photos.insert(1, mk_photo(1, ""));
        // Re-delivery of idx=0 with a different caption — must not bump len.
        buf.photos.insert(0, mk_photo(0, "redelivered"));
        assert_eq!(buf.photos.len() as u32, buf.expected_size);
        assert_eq!(buf.photos.get(&0).unwrap().caption, "redelivered");
    }

    // 16. Partial album where idx=0 is missing → caption attaches nowhere.
    #[test]
    fn plan_flush_partial_album_no_index_zero_caption_nowhere() {
        let mut buf = mk_buf(3, Instant::now() + Duration::from_secs(5));
        buf.photos.insert(1, mk_photo(1, "wide"));
        buf.photos.insert(2, mk_photo(2, "wider"));
        match plan_flush(buf, 5) {
            FlushDecision::Album { items, caption_index, chat_id } => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0].index, 1);
                assert_eq!(items[1].index, 2);
                assert_eq!(caption_index, 0);
                assert_eq!(chat_id, 5);
                let paired = build_album_input(&items, caption_index);
                // No idx==0 present → every pair has None caption.
                for (_, cap) in &paired {
                    assert!(cap.is_none(), "expected no caption on partial album");
                }
            }
            other => panic!("expected Album, got {:?}", other),
        }
    }

    #[test]
    fn delivery_receipt_replay_deduplicates_and_binds_content() {
        let root = std::env::temp_dir().join(format!(
            "corky-telegram-receipts-{}-{}",
            std::process::id(),
            current_epoch_ms()
        ));
        let path = root.join("receipts.jsonl");
        {
            let mut store = DeliveryReceiptStore::open(path.clone()).unwrap();
            store
                .record(DeliveryReceipt {
                    delivery_id: "delivery-1".to_string(),
                    chat_id: 7,
                    text: "critical page".to_string(),
                    delivered_at_ms: 10,
                })
                .unwrap();
        }
        {
            let mut file = OpenOptions::new().append(true).open(&path).unwrap();
            file.write_all(b"{\"delivery_id\":\"interrupted").unwrap();
            file.sync_data().unwrap();
        }
        let store = DeliveryReceiptStore::open(path).unwrap();
        assert_eq!(store.matching("delivery-1", 7, "critical page"), Some(true));
        assert_eq!(store.matching("delivery-1", 7, "changed"), Some(false));
        assert_eq!(store.matching("unknown", 7, "critical page"), None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn delivery_ack_envelope_is_additive_and_stable() {
        let payload = delivery_ack_payload(&DeliveryAck {
            delivery_id: "daily:2026-07-14:+00:00:09:00".to_string(),
            status: DeliveryAckStatus::Delivered,
            detail: Some("already delivered".to_string()),
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(value[0], "ok");
        assert_eq!(value[1], "delivery_ack");
        assert_eq!(value[2]["status"], "delivered");
        assert_eq!(value[2]["delivery_id"], "daily:2026-07-14:+00:00:09:00");
    }
}
