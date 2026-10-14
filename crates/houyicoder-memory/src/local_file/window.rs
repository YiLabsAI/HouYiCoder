//! Tests for the durable session store: the byte-window reads forward and
//! reverse with their error paths and their defaults on the in-memory
//! backend, the append path with its dedup and its legacy directory spelling,
//! the log size, the tolerant read that skips a corrupt line, the checkpoint
//! manifests, and the replay that rebuilds a session from its event log.

#[cfg(test)]
mod tests;
