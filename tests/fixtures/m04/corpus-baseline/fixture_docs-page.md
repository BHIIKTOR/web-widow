The queue is configured in `acme.toml`. Changes take effect after a restart, and an invalid file prevents startup rather than silently falling back.

## Minimal configuration

```
[queue]
workers = 4
lease_seconds = 30
```

`workers` must not exceed the number of database connections. With the default pool of 8, a value of 4 leaves capacity for readers.

## Leases

A worker leases a job for `lease_seconds`. If the worker dies, the lease expires and another worker may take the job. Leases are not renewed automatically; a long job must call `renew()`.

```
job.renew(Duration::from_secs(30))?;
```

Reusing a lease identifier after it expired is an error, and the call returns `LeaseLost`.

Do not set `lease_seconds` below the p99 job duration.