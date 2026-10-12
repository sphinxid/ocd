DELETE FROM `session_context_epoch`;
--> statement-breakpoint
DELETE FROM `session_input`;
--> statement-breakpoint
DELETE FROM `session_message`;
--> statement-breakpoint
DELETE FROM `event`;
--> statement-breakpoint
DELETE FROM `event_sequence`;
--> statement-breakpoint
UPDATE `session` SET `workspace_id` = NULL WHERE `workspace_id` IS NOT NULL;
--> statement-breakpoint
DELETE FROM `workspace`;
