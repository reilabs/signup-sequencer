DROP UNIQUE INDEX idx_unique_insertion_leaf;
DROP UNIQUE INDEX idx_unique_deletion_leaf;

DROP TRIGGER validate_previous_sequence_id_trigger;
DROP FUNCTION validate_previous_sequence_id();