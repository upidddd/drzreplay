#!/bin/sh
export PYTHONPATH="/usr/lib/drzreplay${PYTHONPATH:+:$PYTHONPATH}"
exec python3 -m drzreplay "$@"
