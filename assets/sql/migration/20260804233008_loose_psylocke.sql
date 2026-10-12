DROP INDEX IF EXISTS `session_project_idx`;
--> statement-breakpoint
DROP INDEX IF EXISTS `session_workspace_idx`;
--> statement-breakpoint
DROP INDEX IF EXISTS `session_parent_idx`;
--> statement-breakpoint
DROP INDEX IF EXISTS `session_time_suspended_idx`;
--> statement-breakpoint
ALTER TABLE `session` RENAME TO `session_v2`;
--> statement-breakpoint
CREATE INDEX `session_v2_project_idx` ON `session_v2` (`project_id`);
--> statement-breakpoint
CREATE INDEX `session_v2_workspace_idx` ON `session_v2` (`workspace_id`);
--> statement-breakpoint
CREATE INDEX `session_v2_parent_idx` ON `session_v2` (`parent_id`);
--> statement-breakpoint
CREATE INDEX `session_v2_time_suspended_idx` ON `session_v2` (`time_suspended`) WHERE "session_v2"."time_suspended" is not null;
--> statement-breakpoint
DROP TABLE IF EXISTS `data_migration`;
--> statement-breakpoint
DROP TABLE IF EXISTS `session_context_epoch`;
--> statement-breakpoint
DROP TABLE IF EXISTS `session_input`;
--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `kv` (
          `key` text PRIMARY KEY,
          `value` text NOT NULL,
          `time_created` integer NOT NULL,
          `time_updated` integer NOT NULL
        );
--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `instruction_blob` (
          `hash` text PRIMARY KEY,
          `value` text
        );
--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `instruction_entry` (
          `session_id` text NOT NULL,
          `key` text NOT NULL,
          `value` text,
          `removed` integer DEFAULT false NOT NULL,
          `time_created` integer NOT NULL,
          `time_updated` integer NOT NULL,
          CONSTRAINT `instruction_entry_pk` PRIMARY KEY(`session_id`, `key`),
          CONSTRAINT `fk_instruction_entry_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
        );
--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `instruction_state` (
          `session_id` text PRIMARY KEY,
          `epoch_start` integer NOT NULL,
          `through_seq` integer NOT NULL,
          `initial_values` text NOT NULL,
          `current_values` text NOT NULL,
          CONSTRAINT `fk_instruction_state_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
        );
--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `session_pending` (
          `id` text PRIMARY KEY,
          `session_id` text NOT NULL,
          `type` text NOT NULL,
          `data` text NOT NULL,
          `delivery` text,
          `admitted_seq` integer NOT NULL,
          `time_created` integer NOT NULL,
          CONSTRAINT `fk_session_pending_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
        );
--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `session_v2` (
          `id` text PRIMARY KEY,
          `project_id` text NOT NULL,
          `workspace_id` text,
          `parent_id` text,
          `fork_session_id` text,
          `fork_boundary` text,
          `slug` text NOT NULL,
          `directory` text NOT NULL,
          `path` text,
          `title` text,
          `version` text NOT NULL,
          `share_url` text,
          `summary_additions` integer,
          `summary_deletions` integer,
          `summary_files` integer,
          `summary_diffs` text,
          `metadata` text,
          `cost` real DEFAULT 0 NOT NULL,
          `tokens_input` integer DEFAULT 0 NOT NULL,
          `tokens_output` integer DEFAULT 0 NOT NULL,
          `tokens_reasoning` integer DEFAULT 0 NOT NULL,
          `tokens_cache_read` integer DEFAULT 0 NOT NULL,
          `tokens_cache_write` integer DEFAULT 0 NOT NULL,
          `revert` text,
          `permission` text,
          `agent` text,
          `model` text,
          `time_created` integer NOT NULL,
          `time_updated` integer NOT NULL,
          `time_compacting` integer,
          `time_archived` integer,
          `time_suspended` integer,
          CONSTRAINT `fk_session_v2_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
        );
--> statement-breakpoint
ALTER TABLE `event` ADD `created` integer DEFAULT 0 NOT NULL;
--> statement-breakpoint
CREATE TABLE IF NOT EXISTS `__new_session_message` (
          `id` text PRIMARY KEY,
          `session_id` text NOT NULL,
          `type` text NOT NULL,
          `seq` integer NOT NULL,
          `time_created` integer NOT NULL,
          `time_updated` integer NOT NULL,
          `data` text NOT NULL,
          CONSTRAINT `fk_session_message_session_id_session_v2_id_fk` FOREIGN KEY (`session_id`) REFERENCES `session_v2`(`id`) ON DELETE CASCADE
        );
--> statement-breakpoint
DROP TABLE `session_message`;
--> statement-breakpoint
ALTER TABLE `__new_session_message` RENAME TO `session_message`;
--> statement-breakpoint
CREATE UNIQUE INDEX `session_message_session_seq_idx` ON `session_message` (`session_id`,`seq`);
--> statement-breakpoint
CREATE INDEX `session_message_session_type_seq_idx` ON `session_message` (`session_id`,`type`,`seq`);
--> statement-breakpoint
CREATE INDEX `session_message_session_time_created_id_idx` ON `session_message` (`session_id`,`time_created`,`id`);
--> statement-breakpoint
CREATE INDEX `session_message_time_created_idx` ON `session_message` (`time_created`);
--> statement-breakpoint
CREATE INDEX `session_pending_session_delivery_seq_idx` ON `session_pending` (`session_id`,`delivery`,`admitted_seq`);
--> statement-breakpoint
CREATE UNIQUE INDEX `session_pending_session_compaction_idx` ON `session_pending` (`session_id`) WHERE "session_pending"."type" = 'compaction';
--> statement-breakpoint
CREATE UNIQUE INDEX `session_pending_session_admitted_seq_idx` ON `session_pending` (`session_id`,`admitted_seq`);
--> statement-breakpoint
CREATE INDEX `session_v2_project_idx` ON `session_v2` (`project_id`);
--> statement-breakpoint
CREATE INDEX `session_v2_workspace_idx` ON `session_v2` (`workspace_id`);
--> statement-breakpoint
CREATE INDEX `session_v2_parent_idx` ON `session_v2` (`parent_id`);
--> statement-breakpoint
CREATE INDEX `session_v2_time_suspended_idx` ON `session_v2` (`time_suspended`) WHERE "session_v2"."time_suspended" is not null;
--> statement-breakpoint
DROP TABLE `data_migration`;
--> statement-breakpoint
DROP TABLE `session_context_epoch`;
--> statement-breakpoint
DROP TABLE `session_input`;
