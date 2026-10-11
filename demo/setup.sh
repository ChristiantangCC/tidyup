#!/bin/sh
# Builds the fake home folder the demo tapes record in. Run from the repo root:
#   cargo build --release && sh demo/setup.sh && vhs demo/dashboard.tape
set -e
home=/private/tmp/tidyup-demo
rm -rf "$home"
mkdir -p "$home/code/acme-shop" "$home/code/dashboard"

cat > "$home/code/acme-shop/tidyup.toml" <<'TOML'
[[service]]
name = "web"
cmd = "pnpm dev"
port = 3000

[[service]]
name = "api"
cmd = "uvicorn main:app --port 8080"
port = 8080

[[service]]
name = "worker"
cmd = "pnpm worker"
TOML

mkdir -p "$home/code/.services"
cat > "$home/code/.services/web.sh" <<'SH'
printf '  \033[1m▲ Next.js 16.0.2\033[0m (Turbopack)\n  - Local:   http://localhost:3000\n\n'
sleep 0.6; printf ' \033[32m✓\033[0m Ready in 812ms\n'
sleep 1.2; printf ' GET / \033[32m200\033[0m in 41ms\n'
sleep 0.9; printf ' GET /products \033[32m200\033[0m in 63ms\n'
sleep 1.1; printf ' GET /api/cart \033[32m200\033[0m in 12ms\n'
sleep 60
SH
cat > "$home/code/.services/api.sh" <<'SH'
sleep 0.3; printf 'INFO:     Uvicorn running on \033[1mhttp://0.0.0.0:8080\033[0m\n'
sleep 0.4; printf 'INFO:     Application startup complete.\n'
sleep 1.6; printf 'INFO:     127.0.0.1 - "POST /predict" \033[32m200 OK\033[0m\n'
sleep 60
SH
cat > "$home/code/.services/worker.sh" <<'SH'
sleep 0.5; printf 'worker ready, waiting for jobs\n'
sleep 1.0; printf '\033[32m✓\033[0m sync-products  182ms\n'
sleep 1.3; printf '\033[32m✓\033[0m send-receipts   94ms\n'
sleep 60
SH

# Stand-ins for pnpm and uvicorn so tidyup.toml reads like a real project.
mkdir -p "$home/bin"
printf '#!/bin/sh\ncase "$1" in dev) exec sh %s/code/.services/web.sh;; worker) exec sh %s/code/.services/worker.sh;; esac\n' "$home" "$home" > "$home/bin/pnpm"
printf '#!/bin/sh\nexec sh %s/code/.services/api.sh\n' "$home" > "$home/bin/uvicorn"
chmod +x "$home/bin/pnpm" "$home/bin/uvicorn"

# Sourced (hidden) at the start of each tape.
cat > "$home/env.sh" <<SH
export PATH="$PWD/target/release:$home/bin:\$PATH"
export PS1='\[\e[36m\]\w\[\e[0m\] \[\e[35m\]❯\[\e[0m\] '
cd ~/code
SH
