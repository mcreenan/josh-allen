These compact canonical Allen-REPLAY/3 seeds cover an empty completed execution,
a tool entry with canonical Ok bytes and a stopped final channel, and a closed
provider Err with a terminal final channel. The fuzz target also synthesizes
typed current Result outcomes and every generated tool Error wrapper variant for
every input, then subjects the canonical document to mutation families.
