UPDATE `credential`
        SET `value` = json_remove(json_set(`value`, '$.type', 'external'), '$.access', '$.refresh', '$.expires')
        WHERE `integration_id` = 'azure'
          AND json_extract(`value`, '$.type') = 'oauth'
          AND json_extract(`value`, '$.methodID') = 'azure-cli'
