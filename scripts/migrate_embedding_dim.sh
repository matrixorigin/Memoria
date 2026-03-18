#!/usr/bin/env bash
set -euo pipefail

DB_HOST="127.0.0.1"
DB_PORT="7777"
DB_USER="root"
DB_PASS="111"
DB_NAME="memoria"
API_URL="https://api.siliconflow.cn/v1"
API_KEY="sk-zpkxkyamuaeqmpbyuorgcfyyxildtbzeftixwevzkgkwaeky"
MODEL="BAAI/bge-m3"

mysql_cmd="mysql -h $DB_HOST -P $DB_PORT -u $DB_USER -p$DB_PASS $DB_NAME -N"

echo "=== Step 1: ALTER columns to vecf32(1024) ==="
$mysql_cmd -e "ALTER TABLE mem_memories MODIFY COLUMN embedding vecf32(1024) DEFAULT NULL;"
echo "  mem_memories done"
$mysql_cmd -e "ALTER TABLE memory_graph_nodes MODIFY COLUMN embedding vecf32(1024) DEFAULT NULL;"
echo "  memory_graph_nodes done"

echo ""
echo "=== Step 2: Re-embed mem_memories ==="
$mysql_cmd -e "SELECT memory_id, content FROM mem_memories WHERE is_active=1;" | while IFS=$'\t' read -r mid content; do
  # Call embedding API
  escaped=$(echo "$content" | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read().strip()))')
  resp=$(curl -s "$API_URL/embeddings" \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"model\":\"$MODEL\",\"input\":$escaped}")
  
  emb=$(echo "$resp" | python3 -c 'import sys,json; d=json.load(sys.stdin); print("["+",".join(str(x) for x in d["data"][0]["embedding"])+"]")')
  dim=$(echo "$resp" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)["data"][0]["embedding"]))')
  
  $mysql_cmd -e "UPDATE mem_memories SET embedding='$emb' WHERE memory_id='$mid';"
  echo "  $mid (${dim}d)"
done

echo ""
echo "=== Step 3: Re-embed memory_graph_nodes ==="
$mysql_cmd -e "SELECT node_id, content FROM memory_graph_nodes WHERE is_active=1;" | while IFS=$'\t' read -r nid content; do
  escaped=$(echo "$content" | python3 -c 'import sys,json; print(json.dumps(sys.stdin.read().strip()))')
  resp=$(curl -s "$API_URL/embeddings" \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" \
    -d "{\"model\":\"$MODEL\",\"input\":$escaped}")
  
  emb=$(echo "$resp" | python3 -c 'import sys,json; d=json.load(sys.stdin); print("["+",".join(str(x) for x in d["data"][0]["embedding"])+"]")')
  dim=$(echo "$resp" | python3 -c 'import sys,json; print(len(json.load(sys.stdin)["data"][0]["embedding"]))')
  
  $mysql_cmd -e "UPDATE memory_graph_nodes SET embedding='$emb' WHERE node_id='$nid';"
  echo "  $nid (${dim}d)"
done

echo ""
echo "=== Step 4: Verify ==="
$mysql_cmd -e "SELECT memory_id, vector_dims(embedding) as dim FROM mem_memories WHERE is_active=1;"
$mysql_cmd -e "SELECT node_id, vector_dims(embedding) as dim FROM memory_graph_nodes WHERE is_active=1;"
echo ""
echo "Done!"
