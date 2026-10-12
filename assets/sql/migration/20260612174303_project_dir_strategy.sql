ALTER TABLE `project_directory` ADD `strategy` text;
--> statement-breakpoint
PRAGMA foreign_keys=OFF;
--> statement-breakpoint
CREATE TABLE `__new_project_directory` (
          `project_id` text NOT NULL,
          `directory` text NOT NULL,
          `type` text,
          `strategy` text,
          `time_created` integer NOT NULL,
          CONSTRAINT `project_directory_pk` PRIMARY KEY(`project_id`, `directory`),
          CONSTRAINT `fk_project_directory_project_id_project_id_fk` FOREIGN KEY (`project_id`) REFERENCES `project`(`id`) ON DELETE CASCADE
        );
--> statement-breakpoint
INSERT INTO `__new_project_directory`(`project_id`, `directory`, `type`, `time_created`) SELECT `project_id`, `directory`, `type`, `time_created` FROM `project_directory`;
--> statement-breakpoint
DROP TABLE `project_directory`;
--> statement-breakpoint
ALTER TABLE `__new_project_directory` RENAME TO `project_directory`;
--> statement-breakpoint
PRAGMA foreign_keys=ON;
