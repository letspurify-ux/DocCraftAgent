#!/bin/sh
set -eu
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

if ! command -v mariadb-admin >/dev/null 2>&1; then
  echo "MariaDB를 시작할 수 없습니다: mariadb-admin 명령을 찾을 수 없습니다." >&2
  exit 1
fi

if ! mariadb-admin --protocol=tcp --host=127.0.0.1 --port=3306 ping --silent >/dev/null 2>&1; then
  if ! command -v brew >/dev/null 2>&1 || ! brew list --versions mariadb >/dev/null 2>&1; then
    echo "MariaDB를 시작할 수 없습니다: Homebrew mariadb가 설치되어 있지 않습니다." >&2
    exit 1
  fi
  echo "MariaDB 서비스를 시작합니다."
  brew services start mariadb

  attempts=0
  until mariadb-admin --protocol=tcp --host=127.0.0.1 --port=3306 ping --silent >/dev/null 2>&1; do
    attempts=$((attempts + 1))
    if [ "$attempts" -ge 20 ]; then
      echo "MariaDB가 20초 안에 준비되지 않았습니다. brew services list를 확인하세요." >&2
      exit 1
    fi
    sleep 1
  done
fi

exec node "$SCRIPT_DIR/scripts/start-all.mjs"
