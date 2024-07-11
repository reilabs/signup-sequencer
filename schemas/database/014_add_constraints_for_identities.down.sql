DROP UNIQUE INDEX idx_unique_insertion_leaf;
DROP UNIQUE INDEX idx_unique_deletion_leaf;

ALTER TABLE identities
    DROP CONSTRAINT identities_root_key;
CREATE INDEX identities_root ON identities (root);

DROP TRIGGER validate_pre_root_trigger;
DROP FUNCTION validate_pre_root();

ALTER TABLE identities DROP COLUMN pre_root;