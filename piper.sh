#!/bin/bash

# Go to piper directory
cd ./piper

# Enter python env
if ( ! source env/bin/activate ); then
    # Create env if none exist
    python3 -m venv env

    source env/bin/activate
fi

# Make sure packages are installed
python3 -m pip install piper-tts flask

# Serve http server
if ( ! python3 -m piper.http_server -m en_US-ryan-high ); then
    # If fails, assume model is not downloaded
    python3 -m piper.download_voices en_US-ryan-high

    python3 -m piper.http_server -m en_US-ryan-high
fi
