#!/bin/bash

# Check for Files
if [ ! -f "oresat-watchdog.service" ] || [ ! -f "oresat-c3.service" ]; then
    echo "Error: Service files not found in current directory."
    echo "Please ensure oresat-watchdog.service and oresat-c3.service are in this folder."
    exit 1
fi

# Stop Existing Service
echo "Stopping any existing services..."
sudo systemctl stop oresat-c3.service 2>/dev/null
sudo systemctl stop oresat-watchdog.service 2>/dev/null

# Copy Files to Systemd
echo "Installing unit files to /etc/systemd/system/..."
sudo cp oresat-watchdog.service /etc/systemd/system/
sudo cp oresat-c3.service /etc/systemd/system/

# Reload Systemd for New Files
echo "Reloading systemd daemon..."
sudo systemctl daemon-reload

# Enable and Start Watchdog
echo "Starting Watchdog Service..."
sudo systemctl enable oresat-watchdog
sudo systemctl start oresat-watchdog

# Enable and Start C3 App
echo "Starting Flight Software..."
sudo systemctl enable oresat-c3
sudo systemctl start oresat-c3

# Status Check
echo "--- STATUS CHECK ---"
echo "1. Watchdog Status (Should be Active/Running):"
systemctl status oresat-watchdog --no-pager | grep "Active:"
echo ""
echo "2. Flight Software Status (Should be Active/Running):"
systemctl status oresat-c3 --no-pager | grep "Active:"