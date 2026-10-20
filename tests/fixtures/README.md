# Previous V1 database fixture

`previous_v1.sqlite3` was created with `ktask-dev` built from commit
`f6fb6ef60c37256b2e53097af23214a13f7dd9de` (package version 0.6.1).
In a private sandbox, that binary ran:

```sh
ktask-dev --json add --title 'Retained task' --goal 'Retained goal' --criterion 'Retained check'
```

The fixture then gained one failed attempt and its event through SQLite, using the
previous schema. SQLite checkpointed the database and changed journal mode to
`DELETE` before the single database file was copied here. The integration test
copies this file into its own sandbox and changes only `projects.root_path` to
the sandbox's path before taking the retained-row snapshot. Its migration ledger
must match `previous_v1.sql`, which is the V1 schema from that commit.
