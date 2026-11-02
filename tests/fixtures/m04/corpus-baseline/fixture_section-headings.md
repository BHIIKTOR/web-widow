These notes record the two thresholds we actually hit and the checks we run before changing engines.

## 1. When a single file is enough

Copying one file is a backup an operator can reason about, and restoring it is a single command that either works or fails loudly in front of the person running it.

A file-based store needs no daemon, no configuration file, no supervision and no upgrade window, which matters for a service that one part-time operator maintains.

Reader concurrency is cheap when the storage engine pages through a single file, and our read-to-write ratio is roughly forty to one in production traffic.

Everything lives on one host, so there is no second machine that needs the data and no reason to pay for a network protocol between the application and its rows.

## 2. When a server earns its keep

Several processes that must write at the same instant need one arbiter that holds the lock and queues the rest, which a single file cannot provide by itself.

A file cannot be shared across machines safely, so network-separated clients are the second reason to run a server process at all.

## 3. Checklist before migrating

Measure write concurrency for a full week, including the nightly batch, before concluding that a file is the bottleneck.

Restore a backup into a scratch directory and query it; a backup that merely completes is not a backup.

Write the rollback plan first, so the migration is reversible on the day it goes wrong.