# docker-bell

`docker-bell` is a small webhook helper for Docker Compose projects. It watches
running containers that opt in with a `bell.token` label, then lets you rebuild
and restart the matching Compose service through a local HTTP endpoint.

I use it for the simple case where a deployment target is already running a
Compose stack and just needs a nudge after a push or CI job. It is not trying to
be a full deployment system.

## How it works

Every managed container needs a `bell.token` label. Docker Compose already adds
the project, service, config file, and working directory labels that
`docker-bell` needs to run the right `docker compose` command later.

For example:

```yaml
services:
  web:
    image: example/web
    labels:
      bell.token: "change-me"
```

When `docker-bell` receives a rebuild request for that token, it runs roughly:

```sh
docker compose \
  --project-directory <compose-working-dir> \
  --file <compose-config-file> \
  --project-name <compose-project> \
  build --no-cache --quiet <service>

docker compose \
  --project-directory <compose-working-dir> \
  --file <compose-config-file> \
  --project-name <compose-project> \
  up --detach
```

If a branch is included in the request, it first runs:

```sh
git -C <compose-working-dir> pull --ff-only <remote> <branch>
```

## Running it

Build it with Cargo:

```sh
cargo build --release
```

Run it directly:

```sh
./target/release/docker-bell
```

By default it listens on `0.0.0.0:8080` and talks to Docker through
`/var/run/docker.sock`. Both can be changed:

```sh
docker-bell --address 127.0.0.1:8080 --docker-path /var/run/docker.sock
```

There is also a basic `docker-bell.service` file in this repository if you want
to run it under systemd.

## Webhook request

Send a `POST` request to `/rebuild`:

```sh
curl -X POST http://127.0.0.1:8080/rebuild \
  -H 'content-type: application/json' \
  -d '{
    "key": "change-me",
    "branch": "main"
  }'
```

Request fields:

- `key`: the value of the container's `bell.token` label.
- `branch`: optional Git branch to pull before rebuilding.
- `remote`: optional Git remote, defaults to `origin`.
- `build_args`: optional extra arguments passed to `docker compose build`.
- `all_services`: optional boolean. When `false`, only the matching Compose
  service is built. When `true`, the service name is omitted from the build
  command.

A successful rebuild returns `OK`. If no container with the token is known,
the endpoint returns `404`.

## Notes

`docker-bell` keeps an in-memory map of labeled containers and updates it from
Docker events. If the Docker event stream drops, it reconnects automatically.
On a cache miss, it asks Docker for the current labeled containers before
returning `404`, which helps when the service starts after the containers are
already running.

Treat the token like a secret. The HTTP endpoint can trigger `git pull`,
`docker compose build`, and `docker compose up`, so put it behind the network
boundary or reverse proxy policy you actually trust.

## License

MIT
