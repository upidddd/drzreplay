import os
import sys

os.execv("/usr/bin/drzreplay", ["drzreplay"] + sys.argv[1:])
