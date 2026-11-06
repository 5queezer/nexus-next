-- The owner the export belongs to. Responses still carry the ID as `userId`, and events record the
-- email as `actor`, which their request hashes cover. Written by `nexus import`.
ALTER TABLE import_meta ADD COLUMN owner_id TEXT;
ALTER TABLE import_meta ADD COLUMN owner_email TEXT;
