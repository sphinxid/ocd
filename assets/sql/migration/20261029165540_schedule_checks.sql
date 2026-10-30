ALTER TABLE `schedule` ADD `check` text;
--> statement-breakpoint
ALTER TABLE `schedule` ADD `checks` integer DEFAULT 0 NOT NULL;
--> statement-breakpoint
ALTER TABLE `schedule` ADD `failures` integer DEFAULT 0 NOT NULL;
--> statement-breakpoint
ALTER TABLE `schedule` ADD `state` text;
