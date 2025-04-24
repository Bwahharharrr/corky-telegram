# Corky Telegram Bot

A Telegram bot that uses ZMQ for inter-process communication, allowing other applications to send messages through the bot to Telegram chats. The bot acts as a bridge between your services and Telegram, enabling automated notifications and messages from your systems to Telegram users or groups.

This bot has been tested and is compatible with [corky-service-zmq](https://github.com/Bwahharharrr/corky-service-zmq).

## Installation

The `install.sh` script performs the following actions:

1. Sets up a configuration directory at `~/.corky/`
2. Creates a configuration file based on `example_config.toml`
3. Validates the configuration to ensure required fields are set
4. Builds the release version of the application
5. Installs the application as either a user-level or system-level systemd service
6. Enables and starts the service

To install:

```bash
./install.sh
```

During installation, you will be prompted to:
- Edit your configuration file with your Telegram bot token and chat IDs
- Choose between user-level or system-level installation

## Uninstallation

The `uninstall.sh` script performs the following actions:

1. Stops the running service
2. Disables the service from starting at boot
3. Removes the service file
4. Reloads systemd configuration

To uninstall:

```bash
./uninstall.sh
```

**Note:** The uninstall script does not remove the bot executable or configuration files.

## Service Management

### User-Level Service

If you installed as a user-level service:

```bash
# Check status
systemctl --user status corky-telegram.service

# Start service
systemctl --user start corky-telegram.service

# Stop service
systemctl --user stop corky-telegram.service

# Restart service
systemctl --user restart corky-telegram.service

# Reload service configuration
systemctl --user daemon-reload
systemctl --user reload corky-telegram.service

# View logs (follow mode)
journalctl --user -u corky-telegram.service -f

# View all logs
journalctl --user -u corky-telegram.service

# View recent logs
journalctl --user -u corky-telegram.service -n 50
```

### System-Level Service

If you installed as a system-level service:

```bash
# Check status
sudo systemctl status corky-telegram.service

# Start service
sudo systemctl start corky-telegram.service

# Stop service
sudo systemctl stop corky-telegram.service

# Restart service
sudo systemctl restart corky-telegram.service

# Reload service configuration
sudo systemctl daemon-reload
sudo systemctl reload corky-telegram.service

# View logs (follow mode)
sudo journalctl -u corky-telegram.service -f

# View all logs
sudo journalctl -u corky-telegram.service

# View recent logs
sudo journalctl -u corky-telegram.service -n 50
```

## ZMQ Communication

The bot uses ZMQ for inter-process communication with the following characteristics:

- Uses a DEALER socket in the Rust application to receive messages
- Expects messages in the format:
  ```
  socket.send_multipart([str.encode(destination), str.encode(msg)])
  ```
  Where `msg` is a JSON-encoded array: `[status, action, data]`
  
- The `data` component should contain:
  - `text`: The message text to send
  - `chat_id` (optional): Specific chat ID to send the message to
  - `subscriber_list` (optional): Name of a subscriber list to send the message to
  - `image_path` (optional): Path to an image file to send with the message

- If neither `chat_id` nor `subscriber_list` is specified, the message will be sent to the owner's chat ID

For examples of how to send different types of messages to the bot, see the included `test.py` script. This script demonstrates sending messages to specific chat IDs, subscriber lists, and more.

## Configuration

The bot is configured through the `~/.corky/config.toml` file. You can edit this file at any time:

```bash
nano ~/.corky/config.toml
```

After changing the configuration, restart the service for changes to take effect:

```bash
# For user-level service
systemctl --user restart corky-telegram.service

# For system-level service
sudo systemctl restart corky-telegram.service
```
