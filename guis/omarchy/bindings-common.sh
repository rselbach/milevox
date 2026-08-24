#!/usr/bin/env bash

readonly BINDINGS_BEGIN="-- milevox:begin"
readonly BINDINGS_END="-- milevox:end"

# A valid file has no exact markers, or one non-nested, ordered exact pair.
validate_bindings_markers() {
  local file=$1 state=0 line
  [[ -f $file ]] || return 0
  while IFS= read -r line || [[ -n $line ]]; do
    if [[ $line == "$BINDINGS_BEGIN" ]]; then
      (( state == 0 )) || return 1
      state=1
    elif [[ $line == "$BINDINGS_END" ]]; then
      (( state == 1 )) || return 1
      state=2
    fi
  done < "$file"
  (( state == 0 || state == 2 ))
}

strip_bindings_block() {
  awk -v begin="$BINDINGS_BEGIN" -v end="$BINDINGS_END" \
    '$0 == begin { inside=1; next } $0 == end { inside=0; next } !inside' "$1"
}

# Print lines outside our block which bind either requested key.  Deliberately
# recognise binding calls, rather than arbitrary occurrences of the key text.
binding_conflicts() {
  local file=$1 first=$2 second=$3
  strip_bindings_block "$file" | awk -v a="$first" -v b="$second" '
    /^[[:space:]]*(o|hl)\.bind[[:space:]]*\(/ {
      line=$0; sub(/^[^"]*"/, "", line); sub(/".*/, "", line)
      if (line == a || line == b) print
    }'
}

remove_binding_conflicts() {
  local first=$2 second=$3
  awk -v a="$first" -v b="$second" '
    /^[[:space:]]*(o|hl)\.bind[[:space:]]*\(/ {
      line=$0; sub(/^[^"]*"/, "", line); sub(/".*/, "", line)
      if (line == a || line == b) next
    } { print }' "$1"
}

stage_for() {
  mktemp "$(dirname -- "$1")/.milevox.XXXXXX"
}

atomic_replace() {
  local staged=$1 destination=$2 mode
  mode=$(stat -c %a -- "$destination") || return
  chmod "$mode" "$staged" && mv -f -- "$staged" "$destination"
}

append_binding_block() {
  local file=$1 toggle=$2 push_to_talk=$3 last_byte
  if [[ -s $file ]]; then
    last_byte=$(tail -c 1 -- "$file" | od -An -t u1)
    [[ $last_byte =~ (^|[[:space:]])10[[:space:]]*$ ]] || printf '\n' >> "$file"
  fi
  printf '%s\nhl.unbind("%s")\nhl.unbind("%s")\no.bind("%s", "Toggle Milevox dictation", "milevox record toggle", { release = true })\no.bind("%s", "Start Milevox dictation", "milevox record start")\no.bind("%s", "Stop Milevox dictation", "milevox record stop", { release = true })\n%s\n' \
    "$BINDINGS_BEGIN" "$toggle" "$push_to_talk" "$toggle" \
    "$push_to_talk" "$push_to_talk" "$BINDINGS_END" >> "$file"
}

new_config_errors() {
  local baseline=$1 after=$2 line
  while IFS= read -r line || [[ -n $line ]]; do
    [[ -z $line ]] && continue
    grep -Fqx -- "$line" <<< "$baseline" || printf '%s\n' "$line"
  done <<< "$after"
}
