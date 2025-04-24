#!/bin/bash

# Colors for output
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

echo -e "${GREEN}Uninstalling Corky Telegram Bot service...${NC}"

# Determine which service type to uninstall
echo -e "${BLUE}Which service would you like to uninstall?${NC}"
echo -e "1) ${YELLOW}System service${NC} (requires sudo)"
echo -e "2) ${YELLOW}User service${NC} (no sudo required)"
read -p "Choose [1/2]: " choice

if [ "$choice" == "1" ]; then
    # System-level service uninstallation
    echo -e "${GREEN}Uninstalling system-level service...${NC}"
    USER_LEVEL=false
    
    # Check if running as root
    if [ "$EUID" -ne 0 ]; then
        echo -e "${YELLOW}Not running as root. Will use sudo for system operations.${NC}"
        SUDO="sudo"
    else
        SUDO=""
    fi
else
    # User-level service uninstallation
    echo -e "${GREEN}Uninstalling user-level service...${NC}"
    USER_LEVEL=true
    SUDO=""
fi

# Stop and disable the service
if [ "$USER_LEVEL" = true ]; then
    # User-level service
    echo -e "${GREEN}Stopping and disabling user service...${NC}"
    if systemctl --user is-active --quiet corky-telegram.service; then
        systemctl --user stop corky-telegram.service
        echo -e "${GREEN}User service stopped.${NC}"
    else
        echo -e "${YELLOW}User service is not running.${NC}"
    fi

    systemctl --user disable corky-telegram.service 2>/dev/null || true
    echo -e "${GREEN}User service disabled.${NC}"

    # Remove service file
    echo -e "${GREEN}Removing user service file...${NC}"
    SERVICE_FILE="$HOME/.config/systemd/user/corky-telegram.service"
    if [ -f "$SERVICE_FILE" ]; then
        rm "$SERVICE_FILE"
        echo -e "${GREEN}User service file removed.${NC}"
    else
        echo -e "${YELLOW}User service file not found at $SERVICE_FILE.${NC}"
    fi

    # Reload systemd
    echo -e "${GREEN}Reloading user systemd configuration...${NC}"
    systemctl --user daemon-reload
else
    # System-level service
    echo -e "${GREEN}Stopping and disabling system service...${NC}"
    if $SUDO systemctl is-active --quiet corky-telegram.service; then
        $SUDO systemctl stop corky-telegram.service
        echo -e "${GREEN}System service stopped.${NC}"
    else
        echo -e "${YELLOW}System service is not running.${NC}"
    fi

    $SUDO systemctl disable corky-telegram.service 2>/dev/null || true
    echo -e "${GREEN}System service disabled.${NC}"

    # Remove service file
    echo -e "${GREEN}Removing system service file...${NC}"
    if [ -f /etc/systemd/system/corky-telegram.service ]; then
        $SUDO rm /etc/systemd/system/corky-telegram.service
        echo -e "${GREEN}System service file removed.${NC}"
    else
        echo -e "${YELLOW}System service file not found.${NC}"
    fi

    # Reload systemd
    echo -e "${GREEN}Reloading system systemd configuration...${NC}"
    $SUDO systemctl daemon-reload
fi

echo -e "${GREEN}Uninstallation complete!${NC}"
echo -e "${YELLOW}Note: The bot executable and configuration files have not been removed.${NC}"
