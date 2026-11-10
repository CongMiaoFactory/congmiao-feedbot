-- Earlier builds recorded `/cancel` as a failure with last_error = 'cancelled'.
UPDATE jobs SET state = 'cancelled', last_error = NULL
WHERE state = 'failed' AND last_error = 'cancelled';
