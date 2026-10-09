DROP TABLE user_recovery_codes;
DROP TABLE user_mfa;
ALTER TABLE user_sessions DROP COLUMN auth_method;
