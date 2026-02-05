use teloxide::{prelude::*, types::InputFile};
use serde::Deserialize;
use log::{error, info, warn, trace, Level, LevelFilter, Metadata, Record};
use chrono::Local;
use std::{fs, path::PathBuf, collections::HashMap, thread};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::{signal, sync::mpsc, time};

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
}

/// Events sent to the central channel
enum Event {
    Zmq(Vec<Vec<u8>>),
    Shutdown,
}

/// Parse and handle raw ZMQ frames
async fn handle_zmq_frames(
    bot: &Bot,
    settings: &config::TelegramSettings,
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
                                process_zmq_message(bot, settings, cmd).await
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

/// Dispatch ZMQ command to appropriate chats
async fn process_zmq_message(
    bot: &Bot,
    settings: &config::TelegramSettings,
    cmd: ZmqMessage,
) {
    info!("Processing ZMQ message: {:?}", cmd);

    if let Some(chat_id) = cmd.chat_id {
        if let Some(img_path) = &cmd.image_path {
            send_to_chat_with_image_retry(bot, ChatId(chat_id), &cmd.text, img_path).await;
        } else {
            send_to_chat_with_retry(bot, ChatId(chat_id), &cmd.text).await;
        }
    } else if let Some(list_name) = &cmd.subscriber_list {
        if let Some(subs) = settings.subscriber_lists.get(list_name) {
            for &sub_id in subs {
                if let Some(img_path) = &cmd.image_path {
                    send_to_chat_with_image_retry(bot, ChatId(sub_id), &cmd.text, img_path).await;
                } else {
                    send_to_chat_with_retry(bot, ChatId(sub_id), &cmd.text).await;
                }
            }
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
    const MAX_RETRIES: u8 = 3;
    const BASE_DELAY_MS: u64 = 500;
    
    for attempt in 0..MAX_RETRIES {
        match time::timeout(
            time::Duration::from_secs(30),
            bot.send_message(chat, text),
        ).await {
            Ok(Ok(_)) => {
                info!("Sent message to {}: \"{}\"", chat, if text.len() > 30 { format!("{}...", truncate_str(text, 30)) } else { text.to_string() });
                return;
            }
            Ok(Err(err)) => {
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
                if attempt < MAX_RETRIES - 1 {
                    warn!("Timeout sending to {} (attempt {}/{}), retrying", chat, attempt + 1, MAX_RETRIES);
                } else {
                    error!("Timeout sending to {} after {} attempts", chat, MAX_RETRIES);
                }
            }
        }
    }
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
                
                // Color coding based on message type and level
                let (color_code, prefix) = match record.level() {
                    Level::Error => ("\x1b[31m", "ERROR"), // Red for errors
                    Level::Warn => ("\x1b[33m", "WARN "), // Yellow for warnings
                    Level::Info => {
                        let msg = record.args().to_string();
                        if msg.contains("ZMQ:") {
                            if msg.contains("received message") || msg.contains("Received message") {
                                ("\x1b[36m", "ZMQ ") // Cyan for ZMQ received messages
                            } else {
                                ("\x1b[90m", "ZMQ ") // Dark gray for other ZMQ messages
                            }
                        } else if msg.contains("telegram") || msg.contains("bot") {
                            ("\x1b[32m", "BOT ") // Green for bot-related messages
                        } else if msg.contains("Processing") || msg.contains("command") {
                            ("\x1b[35m", "CMD ") // Magenta for command processing
                        } else if msg.contains("Sent message") {
                            ("\x1b[34m", "MSG ") // Blue for sent messages
                        } else {
                            ("\x1b[0m", "INFO") // Default for other info messages
                        }
                    }
                    _ => ("\x1b[0m", "INFO"), // Default color for other levels
                };
                
                // Reset color code at the end
                let reset_code = "\x1b[0m";
                
                // Filter and condense ZMQ debug messages
                let message = record.args().to_string();
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
                                    format!("Content: {}", &text_content[..end])
                                } else if let Some(end) = text_content.find("\"}") {
                                    format!("Content: {}", &text_content[..end])
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

    // Central event channel (bounded to prevent unbounded memory growth)
    let (tx, mut rx) = mpsc::channel::<Event>(256);

    // Shutdown flag shared with the ZMQ thread
    let shutdown_flag = Arc::new(AtomicBool::new(false));

    // Spawn ZMQ listener in a dedicated thread
    let zmq_handle = {
        let tx = tx.clone();
        let endpoint = settings.zmq_endpoint.clone();
        let shutdown = shutdown_flag.clone();
        thread::spawn(move || {
            info!("ZMQ: Starting listener thread");

            // Outer reconnection loop
            while !shutdown.load(Ordering::Relaxed) {
                let context = zmq::Context::new();
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
                while consecutive_errors < max_consecutive_errors && !shutdown.load(Ordering::Relaxed) {
                    // Poll with timeout (5 seconds - allows for periodic health checks)
                    match zmq::poll(&mut items, 5000) {
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
                                        if tx.blocking_send(Event::Zmq(frames)).is_err() {
                                            info!("ZMQ: Channel closed, shutting down");
                                            return;
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

                if shutdown.load(Ordering::Relaxed) {
                    break;
                }

                // If we reached max consecutive errors, close socket and reconnect
                error!("ZMQ: Too many consecutive errors ({}), reconnecting...", max_consecutive_errors);
                let _ = socket.disconnect(&endpoint);
                drop(socket);
                drop(context);
                std::thread::sleep(std::time::Duration::from_secs(5));
            }

            info!("ZMQ: Listener thread exiting");
        })
    };

    // Spawn CTRL+C handler
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            if signal::ctrl_c().await.is_ok() {
                let _ = tx.send(Event::Shutdown).await;
            }
        });
    }

    // Telegram command dispatcher (no internal CTRL+C handler)
    let handler = Update::filter_message()
        .filter_command::<commands::Command>()
        .endpoint(commands::handle);
    let mut dispatcher = Dispatcher::builder(bot.clone(), handler).build();
    let _dispatch_task = tokio::spawn(async move {
        dispatcher.dispatch().await;
    });

    // Central event loop: handle ZMQ messages or shutdown
    while let Some(event) = rx.recv().await {
        match event {
            Event::Zmq(frames) => handle_zmq_frames(&bot, &settings, frames).await,
            Event::Shutdown => {
                info!("Shutdown event received; exiting");
                break;
            }
        }
    }

    // Signal the ZMQ thread to stop and wait for it
    shutdown_flag.store(true, Ordering::Relaxed);
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
}
