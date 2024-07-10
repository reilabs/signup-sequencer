CREATE UNIQUE INDEX idx_unique_insertion_leaf on identities(leaf_index) WHERE commitment != E'\\x0000000000000000000000000000000000000000000000000000000000000000';
CREATE UNIQUE INDEX idx_unique_deletion_leaf on identities(leaf_index) WHERE commitment = E'\\x0000000000000000000000000000000000000000000000000000000000000000';

-- Add the new 'prev_id' column
ALTER TABLE identities ADD COLUMN prev_id BIGINT UNIQUE;

-- This constraint ensures that we have consistent database and changes to the tre are done in a valid sequence.
CREATE OR REPLACE FUNCTION validate_previous_sequence_id() returns trigger as $$
    DECLARE
        last_sequence_id BIGINT;
    BEGIN
        last_sequence_id := (
            SELECT id
            FROM identities
            ORDER BY id DESC
            LIMIT 1
        );

        -- When last_sequence_id is NULL that means there are no records in identities table. The first prev_id can
        -- be a null as there is no previous id.
        IF last_sequence_id IS NOT NULL AND NEW.prev_id IS NULL THEN RAISE EXCEPTION 'Field prev_id must be set.';
        END IF;

        IF (last_sequence_id != NEW.prev_id) THEN RAISE EXCEPTION 'Sent prev_id (%) is different than last sequence id (%) in database.', NEW.prev_id, last_sequence_id;
        END IF;

        RETURN NEW;
    END;
$$ language plpgsql;
CREATE TRIGGER validate_previous_sequence_id_trigger BEFORE INSERT ON identities FOR EACH ROW EXECUTE PROCEDURE validate_previous_sequence_id();
