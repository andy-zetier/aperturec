#!/bin/sh

nohup sh -c Xvfb :0 -screen 0 1920x1080x32 &
"$@"
