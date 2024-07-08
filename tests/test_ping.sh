#!/bin/bash

PORT=20001 # same as watchdog
INTERVAL=5 # Seconds
IP=127.0.0.1 # localhost

while true; do echo "Sending packet..."; nc -vzu $IP $PORT; sleep $INTERVAL; done
