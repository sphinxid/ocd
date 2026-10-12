ALTER TABLE `project` ADD `time_active` integer DEFAULT 0 NOT NULL;
--> statement-breakpoint
UPDATE `project` SET `time_active` = `time_updated`;
