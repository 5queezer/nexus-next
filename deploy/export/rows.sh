#!/bin/sh
# Runs on the old host. One read-only snapshot of every row, count and ID sequence, on stdout.
#   ssh <old-host> sh -s -- <db-container> <db-name> < deploy/export/rows.sh > rows.stream
set -eu
docker exec -i "$1" sh -c 'psql -v ON_ERROR_STOP=1 -U "$POSTGRES_USER" -d "$1" -At' sh "$2" <<'SQL'
begin transaction isolation level repeatable read read only;
select '###TABLE Application';
select row_to_json(t)::text from "Application" t order by id;
select '###COUNT Application ' || count(*) from "Application";
select '###TABLE ApplicationEvent';
select row_to_json(t)::text from "ApplicationEvent" t order by id;
select '###COUNT ApplicationEvent ' || count(*) from "ApplicationEvent";
select '###TABLE ApplicationSubmission';
select row_to_json(t)::text from "ApplicationSubmission" t order by id;
select '###COUNT ApplicationSubmission ' || count(*) from "ApplicationSubmission";
select '###TABLE Contact';
select row_to_json(t)::text from "Contact" t order by id;
select '###COUNT Contact ' || count(*) from "Contact";
select '###TABLE Document';
select row_to_json(t)::text from "Document" t order by id;
select '###COUNT Document ' || count(*) from "Document";
select '###TABLE ApplicationToDocument';
select row_to_json(t)::text from "_ApplicationToDocument" t order by 1;
select '###COUNT ApplicationToDocument ' || count(*) from "_ApplicationToDocument";
select '###TABLE CvProfile';
select row_to_json(t)::text from "CvProfile" t order by id;
select '###COUNT CvProfile ' || count(*) from "CvProfile";
select '###TABLE CvPatch';
select row_to_json(t)::text from "CvPatch" t order by id;
select '###COUNT CvPatch ' || count(*) from "CvPatch";
select '###TABLE ScannedEmail';
select row_to_json(t)::text from "ScannedEmail" t order by id;
select '###COUNT ScannedEmail ' || count(*) from "ScannedEmail";
select '###SEQ Application ' || case when is_called then last_value else 0 end from "Application_id_seq";
select '###SEQ ApplicationEvent ' || case when is_called then last_value else 0 end from "ApplicationEvent_id_seq";
select '###SEQ ApplicationSubmission ' || case when is_called then last_value else 0 end from "ApplicationSubmission_id_seq";
select '###SEQ Contact ' || case when is_called then last_value else 0 end from "Contact_id_seq";
select '###SEQ CvPatch ' || case when is_called then last_value else 0 end from "CvPatch_id_seq";
select '###SEQ CvProfile ' || case when is_called then last_value else 0 end from "CvProfile_id_seq";
select '###SEQ Document ' || case when is_called then last_value else 0 end from "Document_id_seq";
select '###SEQ ScannedEmail ' || case when is_called then last_value else 0 end from "ScannedEmail_id_seq";
commit;
SQL
