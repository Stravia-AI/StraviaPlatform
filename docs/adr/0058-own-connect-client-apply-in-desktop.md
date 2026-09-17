# Own Connect Client Apply in Desktop

Stravia Desktop can upsert a Connect Client's local provider config. The standalone server admin API does not write the operator's home directory: a remote Gateway would mutate the wrong machine. Copy-paste snippets remain the server path.

Server previews use portable client paths directly and do not accept or inspect the server process's directory environment. Desktop alone resolves native paths from the local user's environment. Both paths share the same provider patch and merge logic; previews must not call native directory resolution or rewrite serialized configuration to remove machine paths.
