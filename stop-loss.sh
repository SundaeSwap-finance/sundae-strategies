URL="http://localhost:3000"
NETWORK="preview"
TOKEN_A="."
TOKEN_A_DECIMALS=6
TOKEN_B="99b071ce8580d6a3a11b4902145adb8bfd0d2a03935af8cf66403e15.534245525259"
TOKEN_B_DECIMALS=0
EXECUTION_PRICE=0.000168
PROJECT_ID="1"
DISPLAY_NAME="My worker"

config=$(jq -n \
    --arg network "$NETWORK" \
    --arg token_a "$TOKEN_A" \
    --argjson token_a_decimals "$TOKEN_A_DECIMALS" \
    --arg token_b "$TOKEN_B" \
    --argjson token_b_decimals "$TOKEN_B_DECIMALS" \
    --arg sell_token "$TOKEN_A" \
    --argjson execution_price "$EXECUTION_PRICE" \
    '$ARGS.named'
)
echo "$config"

spec=$(jq -n \
    --arg network "$NETWORK" \
    --arg operatorVersion "1" \
    --arg throughputTier "0" \
    --arg displayName "$DISPLAY_NAME" \
    --arg url "file:///workers/stop-loss.wasm" \
    --argjson config "$config" \
    --arg version "1" \
    '$ARGS.named'
)

payload=$(jq -n \
    --arg projectId "$PROJECT_ID" \
    --arg kind "BaliusWorker" \
    --arg spec "$spec" \
    '$ARGS.named'
)

response=$(curl -s "$URL/resources" -H 'Content-Type: application/json' -d "$payload")
echo $response | jq

payload=$(jq -n \
    --arg method "get-signer-key" \
    --argjson params "{}" \
    '$ARGS.named'
)
id=$(jq -r ".id" <(echo "$response"))
response=$(curl -s "$URL/worker/$id" -H 'Content-Type: application/json' -d "$payload")
echo $response | jq