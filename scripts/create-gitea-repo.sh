set -x

script_dir=$(dirname "$0")
username="$1"
repo="$2"
key_dir="$script_dir/../.keys/$username"

kubectl -n gitea port-forward svc/gitea-http 3000:3000 &
svc_pid=$!
sleep 1

if [ "$(curl -s -o /dev/null -w "%{http_code}" -u admin:password \
  http://localhost:3000/api/v1/users/$username)" -eq 200 ]; then
  echo "User $username exists"
else
  mkdir -p "$key_dir"
  ssh-keygen -t ed25519 -q -N "" -C "kubimo" -f "$key_dir/id_ed25519"
  # create user
  curl -u admin:password \
    -X POST "http://localhost:3000/api/v1/admin/users" \
    -H "Content-Type: application/json" \
    -d '{
      "username": "'"$username"'",
      "password": "password",
      "email": "'"$username"'@local.domain"
    }'
  # insert public key
  curl -u admin:password \
    -X POST "http://localhost:3000/api/v1/admin/users/$username/keys" \
    -H "Content-Type: application/json" \
    -d '{
      "title": "kubimo",
      "key": "'"$(cat "$key_dir/id_ed25519.pub")"'"
    }'
fi

# create repo
curl -u admin:password \
  -X POST "http://localhost:3000/api/v1/admin/users/$username/repos" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "'"$repo"'",
    "private": false,
    "auto_init": false
  }'

kill $svc_pid
