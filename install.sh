#!/bin/bash
set -e

# Colors for output
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${GREEN}Installing Corky Telegram Bot service...${NC}"

# Get the current user and directory
CURRENT_USER=$(whoami)
WORK_DIR=$(pwd)

# Get the real user's home directory, even when run with sudo
if [ -n "$SUDO_USER" ]; then
    REAL_USER="$SUDO_USER"
    REAL_HOME=$(eval echo ~$SUDO_USER)
else
    REAL_USER="$CURRENT_USER"
    REAL_HOME="$HOME"
fi

# Handle config file setup
CONFIG_DIR="$REAL_HOME/.corky"
CONFIG_PATH="$CONFIG_DIR/config.toml"
EXAMPLE_CONFIG="$WORK_DIR/example_config.toml"

# Create config directory if it doesn't exist
if [ ! -d "$CONFIG_DIR" ]; then
    echo -e "${YELLOW}Creating config directory at $CONFIG_DIR${NC}"
    mkdir -p "$CONFIG_DIR"
    # If we're running as root, ensure the directory is owned by the real user
    if [ "$EUID" -eq 0 ]; then
        chown "$REAL_USER" "$CONFIG_DIR"
    fi
fi

# Check if config file exists
if [ ! -f "$CONFIG_PATH" ]; then
    echo -e "${YELLOW}Config file not found. Creating from example_config.toml...${NC}"
    cp "$EXAMPLE_CONFIG" "$CONFIG_PATH"
    # If we're running as root, ensure the config file is owned by the real user
    if [ "$EUID" -eq 0 ]; then
        chown "$REAL_USER" "$CONFIG_PATH"
    fi
    echo -e "${GREEN}Created config file at $CONFIG_PATH${NC}"
    echo -e "${YELLOW}You must edit this file with your actual Telegram bot token and chat IDs before continuing.${NC}"
    echo -e "${YELLOW}You can edit it with: nano $CONFIG_PATH${NC}"
    
    read -p "Edit the config file now? [Y/n] " edit_choice
    if [[ ! "$edit_choice" =~ ^[Nn]$ ]]; then
        nano "$CONFIG_PATH"
    else
        echo -e "${RED}Installation paused. Please edit the config file and then run this script again.${NC}"
        exit 1
    fi
    
    echo -e "${YELLOW}Have you updated the config file with your actual Telegram bot token and chat IDs?${NC}"
    read -p "Continue with installation? [y/N] " continue
    if [[ ! "$continue" =~ ^[Yy]$ ]]; then
        echo -e "${RED}Installation aborted. Please run this script again after editing the config file.${NC}"
        exit 1
    fi
else
    echo -e "${GREEN}Found existing config file at $CONFIG_PATH${NC}"
    
    # Check if the config file has the required sections
    MISSING_SECTIONS=0
    
    # Check for [telegram] section
    if ! grep -q '^\[telegram\]' "$CONFIG_PATH"; then
        echo -e "${YELLOW}Adding [telegram] section from example config...${NC}"
        echo "" >> "$CONFIG_PATH"
        sed -n '/^\[telegram\]/,/^\[telegram\.subscriber_lists\]/p' "$EXAMPLE_CONFIG" | grep -v '^\[telegram\.subscriber_lists\]' >> "$CONFIG_PATH"
        MISSING_SECTIONS=1
    fi
    
    # Check for [telegram.subscriber_lists] section
    if ! grep -q '^\[telegram\.subscriber_lists\]' "$CONFIG_PATH"; then
        echo -e "${YELLOW}Adding [telegram.subscriber_lists] section from example config...${NC}"
        echo "" >> "$CONFIG_PATH"
        sed -n '/^\[telegram\.subscriber_lists\]/,$p' "$EXAMPLE_CONFIG" >> "$CONFIG_PATH"
        MISSING_SECTIONS=1
    fi
    
    if [ $MISSING_SECTIONS -eq 1 ]; then
        echo -e "${YELLOW}Updated config file with missing sections.${NC}"
        echo -e "${YELLOW}Please review and edit as needed before continuing.${NC}"
        echo -e "${YELLOW}You can edit it with: nano $CONFIG_PATH${NC}"
        
        read -p "Edit the config file now? [Y/n] " edit_choice
        if [[ ! "$edit_choice" =~ ^[Nn]$ ]]; then
            nano "$CONFIG_PATH"
        fi
        
        read -p "Continue with installation? [y/N] " continue
        if [[ ! "$continue" =~ ^[Yy]$ ]]; then
            echo -e "${RED}Installation aborted. Please run this script again after editing the config file.${NC}"
            exit 1
        fi
    fi
    
    # Verify bot_token and owner_chat_id exist and aren't default values
    if grep -q 'bot_token = "123456789:ABCDEFGHIJKLMNOPQRSTUVWXYZ"' "$CONFIG_PATH"; then
        echo -e "${RED}ERROR: Default bot token detected in config file.${NC}"
        echo -e "${YELLOW}Please update the bot_token in $CONFIG_PATH with your actual Telegram bot token.${NC}"
        echo -e "${YELLOW}You can edit it with: nano $CONFIG_PATH${NC}"
        exit 1
    fi
    
    if grep -q 'owner_chat_id = 123456789' "$CONFIG_PATH"; then
        echo -e "${YELLOW}WARNING: Default owner_chat_id detected in config file.${NC}"
        echo -e "${YELLOW}You may want to update this with your actual Telegram chat ID.${NC}"
        read -p "Continue anyway? [y/N] " continue
        if [[ ! "$continue" =~ ^[Yy]$ ]]; then
            echo -e "${RED}Installation aborted. Please update the config file and try again.${NC}"
            echo -e "${YELLOW}You can edit it with: nano $CONFIG_PATH${NC}"
            exit 1
        fi
    fi
fi

# Default to user-level installation
USER_LEVEL=true

# Ask user for installation preference
echo -e "${BLUE}Would you like to install as a system service (requires sudo) or user service?${NC}"
echo -e "1) ${YELLOW}System service${NC} (starts on boot, requires sudo)"
echo -e "2) ${YELLOW}User service${NC} (starts on login, no sudo required)"
read -p "Choose [1/2]: " choice

if [ "$choice" == "1" ]; then
    USER_LEVEL=false
    
    # Check if running as root, use sudo if not
    if [ "$EUID" -ne 0 ]; then
        echo -e "${YELLOW}Not running as root. Will use sudo for system operations.${NC}"
        SUDO="sudo"
    else
        SUDO=""
    fi
else
    echo -e "${GREEN}Installing as user service...${NC}"
    USER_LEVEL=true
    SUDO=""
    
    # Create user systemd directory if it doesn't exist
    mkdir -p ~/.config/systemd/user/
fi

# Build the release version
echo -e "${GREEN}Building release version...${NC}"
cargo build --release
if [ $? -ne 0 ]; then
    echo -e "${RED}Build failed. Please fix any errors and try again.${NC}"
    exit 1
fi

# Verify binary exists
if [ ! -f "target/release/corky-telegram" ]; then
    echo -e "${RED}Built binary not found at target/release/corky-telegram${NC}"
    exit 1
fi

# Update the service file with the actual paths and user
echo -e "${GREEN}Configuring service file...${NC}"
if [ "$USER_LEVEL" = true ]; then
    # Create a simple user-level service file - simplified for compatibility
    cat > /tmp/corky-telegram.service << EOF
[Unit]
Description=Corky Telegram ZMQ Bot
After=network.target

[Service]
ExecStart=$WORK_DIR/target/release/corky-telegram
WorkingDirectory=$WORK_DIR
Restart=on-failure
RestartSec=5s

[Install]
WantedBy=default.target
EOF
else
    # System-level service with all security options
    sed -e "s|%USER%|$CURRENT_USER|g" \
        -e "s|%WORK_DIR%|$WORK_DIR|g" \
        corky-telegram.service > /tmp/corky-telegram.service
fi

# Install service file to appropriate location
if [ "$USER_LEVEL" = true ]; then
    # User-level service
    echo -e "${GREEN}Installing user-level systemd service...${NC}"
    cp /tmp/corky-telegram.service ~/.config/systemd/user/
    chmod 644 ~/.config/systemd/user/corky-telegram.service
    
    # Reload systemd, enable and start the service
    echo -e "${GREEN}Enabling and starting user service...${NC}"
    systemctl --user daemon-reload
    systemctl --user enable corky-telegram.service
    systemctl --user start corky-telegram.service
    
    # Check if service is running
    sleep 2
    if systemctl --user is-active --quiet corky-telegram.service; then
        echo -e "${GREEN}Service is now running!${NC}"
        echo -e "You can check its status with: ${YELLOW}systemctl --user status corky-telegram.service${NC}"
        echo -e "View logs with: ${YELLOW}journalctl --user -u corky-telegram.service -f${NC}"
    else
        echo -e "${RED}Service failed to start.${NC}"
        echo -e "Check status with: ${YELLOW}systemctl --user status corky-telegram.service${NC}"
        echo -e "View logs with: ${YELLOW}journalctl --user -u corky-telegram.service -f${NC}"
        echo -e "${YELLOW}Checking logs now:${NC}"
        systemctl --user status corky-telegram.service
        exit 1
    fi
else
    # System-level service
    echo -e "${GREEN}Installing system-level systemd service...${NC}"
    $SUDO cp /tmp/corky-telegram.service /etc/systemd/system/
    $SUDO chmod 644 /etc/systemd/system/corky-telegram.service
    
    # Reload systemd, enable and start the service
    echo -e "${GREEN}Enabling and starting system service...${NC}"
    $SUDO systemctl daemon-reload
    $SUDO systemctl enable corky-telegram.service
    $SUDO systemctl start corky-telegram.service
    
    # Check if service is running
    sleep 2
    if $SUDO systemctl is-active --quiet corky-telegram.service; then
        echo -e "${GREEN}Service is now running!${NC}"
        echo -e "You can check its status with: ${YELLOW}sudo systemctl status corky-telegram.service${NC}"
        echo -e "View logs with: ${YELLOW}sudo journalctl -u corky-telegram.service -f${NC}"
    else
        echo -e "${RED}Service failed to start.${NC}"
        echo -e "Check status with: ${YELLOW}sudo systemctl status corky-telegram.service${NC}"
        echo -e "View logs with: ${YELLOW}sudo journalctl -u corky-telegram.service -f${NC}"
        echo -e "${YELLOW}Checking logs now:${NC}"
        $SUDO systemctl status corky-telegram.service
        exit 1
    fi
fi

echo -e "${GREEN}Installation complete!${NC}"
