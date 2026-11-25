ALTER TABLE `machine` ADD `key` text;
--> statement-breakpoint
CREATE UNIQUE INDEX `machine_key_idx` ON `machine` (`key`);
