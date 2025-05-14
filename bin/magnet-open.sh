#!/bin/bash

# Configuration
QBT_HOST="192.168.50.76"
QBT_PORT="8080"
QBT_USER="admin"
QBT_PASS="adminadmin"
MAGNET_LINK="$1"

# Check if magnet link is provided
if [ -z "$MAGNET_LINK" ]; then
  echo "Usage: $0 '<magnet_link>'"
  exit 1
fi

# Temp file to store cookies
COOKIE_JAR=$(mktemp)

# Authenticate and store cookies
LOGIN_RESPONSE=$(curl -s -c "$COOKIE_JAR" \
  --data "username=$QBT_USER&password=$QBT_PASS" \
  "http://$QBT_HOST:$QBT_PORT/api/v2/auth/login")

# Check for successful login
if [[ "$LOGIN_RESPONSE" != "Ok." ]] || ! grep -q $'\tSID\t' "$COOKIE_JAR"; then
  echo "❌ Login failed. Check credentials or Web UI status."
  rm "$COOKIE_JAR"
  exit 1
fi

# Send magnet link
RESPONSE=$(curl -s -b "$COOKIE_JAR" \
  --data-urlencode "urls=$MAGNET_LINK" \
  "http://$QBT_HOST:$QBT_PORT/api/v2/torrents/add")

# Check result
if [[ "$RESPONSE" == "Ok." || -z "$RESPONSE" ]]; then
  echo "✅ Magnet link added successfully."
else
  echo "⚠️ Failed to add magnet link: $RESPONSE"
fi

# Cleanup
rm "$COOKIE_JAR"
