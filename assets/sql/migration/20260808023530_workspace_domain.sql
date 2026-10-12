DROP TABLE `workspace`;
--> statement-breakpoint
CREATE TABLE `workspace` (
          `id` text PRIMARY KEY,
          `provider` text NOT NULL,
          `binding` text NOT NULL,
          `created_at` integer NOT NULL,
          `last_used_at` integer NOT NULL
        );
