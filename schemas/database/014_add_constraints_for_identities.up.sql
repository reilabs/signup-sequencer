CREATE UNIQUE INDEX idx_unique_insertion_leaf on identities(leaf_index) WHERE commitment != E'\\x0000000000000000000000000000000000000000000000000000000000000000';
CREATE UNIQUE INDEX idx_unique_deletion_leaf on identities(leaf_index) WHERE commitment = E'\\x0000000000000000000000000000000000000000000000000000000000000000';

DROP INDEX identities_root;
ALTER TABLE identities
    ADD CONSTRAINT unique_root_key UNIQUE(root);

-- Add the new 'prev_id' column
ALTER TABLE identities ADD COLUMN pre_root BYTEA NOT NULL UNIQUE;

-- This constraint ensures that we have consistent database and changes to the tre are done in a valid sequence.
CREATE OR REPLACE FUNCTION validate_pre_root() returns trigger as $$
    DECLARE
        last_root BYTEA;
    BEGIN
        last_root := (
            SELECT root
            FROM identities
            ORDER BY id DESC
            LIMIT 1
        );

        -- When last_root is NULL that means there are no records in identities table. The first prev_root can
        -- be a value not referencing previous root in database.
        IF last_root IS NULL THEN RETURN NEW;
        END IF;

        IF (last_root != NEW.pre_root) THEN RAISE EXCEPTION 'Sent pre_root (%) is different than last root (%) in database.', NEW.pre_root, last_root;
        END IF;

        RETURN NEW;
    END;
$$ language plpgsql;
CREATE TRIGGER validate_pre_root_trigger BEFORE INSERT ON identities FOR EACH ROW EXECUTE PROCEDURE validate_pre_root();
