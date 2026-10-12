CREATE TABLE `__new_workspace` (
          `id` text PRIMARY KEY,
          `provider` text NOT NULL,
          `binding` text,
          `created_at` integer NOT NULL,
          `last_used_at` integer NOT NULL
        );
--> statement-breakpoint
INSERT INTO `__new_workspace`(`id`, `provider`, `binding`, `created_at`, `last_used_at`) SELECT `id`, `provider`, `binding`, `created_at`, `last_used_at` FROM `workspace`;
--> statement-breakpoint
DROP TABLE `workspace`;
--> statement-breakpoint
ALTER TABLE `__new_workspace` RENAME TO `workspace`;
