#!/bin/sh

set -eu

redis_server=${1:?redis-server path is required}
redis_cli=${2:?redis-cli path is required}
mode=${3:-small}
instance=${4:-$$}
root=/tmp/hermit-redis-strict-$instance
port=18941

case "$mode" in
  small|extended) ;;
  *)
    printf 'unknown workload mode: %s\n' "$mode" >&2
    exit 2
    ;;
esac

cleanup() {
  "$redis_cli" -h 127.0.0.1 -p "$port" shutdown nosave \
    >/dev/null 2>&1 || true
  rm -rf "$root"
}
trap cleanup EXIT HUP INT TERM

mkdir "$root"

start_server() {
  "$redis_server" \
    --daemonize yes \
    --bind 127.0.0.1 \
    --protected-mode no \
    --port "$port" \
    --save '' \
    --appendonly no \
    --dbfilename dump.rdb \
    --pidfile "$root/redis.pid" \
    --logfile "$root/redis.log" \
    --dir "$root"

  attempt=0
  until "$redis_cli" -h 127.0.0.1 -p "$port" ping >/dev/null 2>&1; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 100 ]; then
      printf 'redis-server did not become ready\n' >&2
      cat "$root/redis.log" >&2
      exit 1
    fi
    sleep 0.02
  done
}

redis() {
  "$redis_cli" --raw -h 127.0.0.1 -p "$port" "$@"
}

expect() {
  expected=$1
  shift
  actual=$(redis "$@")
  if [ "$actual" != "$expected" ]; then
    printf 'redis command failed: expected <%s>, got <%s>\n' \
      "$expected" "$actual" >&2
    exit 1
  fi
}

start_server
expect PONG PING
expect OK SET strict:string hermit
expect hermit GET strict:string
expect 1 INCR strict:counter
expect 2 INCR strict:counter
expect 3 RPUSH strict:list alpha beta gamma
expect "$(printf 'alpha\nbeta\ngamma')" LRANGE strict:list 0 -1

printf 'mode=%s\n' "$mode"
printf 'ping=PONG\n'
printf 'string=hermit\n'
printf 'counter=2\n'
printf 'list=alpha,beta,gamma\n'

if [ "$mode" = extended ]; then
  expect 2 HSET strict:hash field-a one field-b two
  expect one HGET strict:hash field-a
  expect two HGET strict:hash field-b
  expect 3 SADD strict:set gamma alpha beta
  expect 1 SISMEMBER strict:set alpha
  expect 1 SISMEMBER strict:set beta
  expect 1 SISMEMBER strict:set gamma
  expect 3 ZADD strict:zset 30 gamma 10 alpha 20 beta
  expect "$(printf 'alpha\nbeta\ngamma')" ZRANGE strict:zset 0 -1
  expect 42 EVAL 'return redis.call("INCRBY", KEYS[1], ARGV[1])' \
    1 strict:lua-counter 42
  expect '1-0' XADD strict:stream 1-0 field value
  expect "$(printf 'strict:stream\n1-0\nfield\nvalue')" \
    XREAD COUNT 1 STREAMS strict:stream 0-0

  redis BGSAVE >/dev/null
  attempt=0
  while [ ! -s "$root/dump.rdb" ]; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 200 ]; then
      printf 'redis background save did not finish\n' >&2
      cat "$root/redis.log" >&2
      exit 1
    fi
    sleep 0.02
  done

  redis SHUTDOWN NOSAVE >/dev/null
  start_server
  expect hermit GET strict:string
  expect 2 GET strict:counter
  printf 'persistence=ok\n'
fi

redis SHUTDOWN NOSAVE >/dev/null
trap - EXIT HUP INT TERM
rm -rf "$root"
printf 'redis-strict-%s-ok\n' "$mode"
