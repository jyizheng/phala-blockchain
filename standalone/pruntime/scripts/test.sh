#!/bin/bash

set -x 


ENDPOINT='http://127.0.0.1:8000'
rand_number=$((RANDOM))
data="${2}"

if [ -z "${data}" ]; then
    data='{}'
fi

curl -sgX POST -H "Content-Type: application/json" "${ENDPOINT}/${1}" \
     -d "{\"input\":${data}, \"nonce\": {\"id\": ${rand_number}}}"

