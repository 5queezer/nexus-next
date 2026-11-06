#!/bin/sh
# Runs on the old host. Custom-format dump of the allowlisted tables, on stdout.
#   ssh <old-host> sh -s -- <db-container> <db-name> < deploy/export/dump.sh > nexuscrm.dump
# Never add a credential table to this list.
set -eu
docker exec "$1" sh -c 'pg_dump -U "$POSTGRES_USER" -d "$1" -Fc --no-owner --no-privileges \
  -t "\"Application\"" -t "\"ApplicationEvent\"" -t "\"ApplicationSubmission\"" -t "\"Contact\"" \
  -t "\"Document\"" -t "\"_ApplicationToDocument\"" -t "\"CvProfile\"" -t "\"CvPatch\"" -t "\"ScannedEmail\""' sh "$2"
