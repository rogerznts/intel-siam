#!/usr/bin/env python3
# Replicates the SimPlay 1.4.1 cloud login (OAuth2 PKCE, loopback redirect) and
# fetches the device list the way the app does. Opens the system browser; you log
# in with your Intelbras account. No app code or DB involved.
#
#   INTEL_SIAM_CLIENT_ID=... python3 oauth_login.py
#
# Writes the raw /video-devices JSON to a file given by RAW_OUT (default: a
# scratchpad path) and prints a redacted summary. Device passwords are secrets.
import base64, hashlib, http.server, json, os, secrets, string, sys, threading
import urllib.parse, urllib.request, webbrowser

AUTH = "https://api.conta.intelbras.com"
API  = "https://api.simplay.isec.intelbras.com.br/api/"
CLIENT_ID = os.environ["INTEL_SIAM_CLIENT_ID"]
APP_VERSION = "1.4"
RAW_OUT = os.environ.get("RAW_OUT", "/private/tmp/video-devices.json")

ALPHABET = string.ascii_letters + string.digits
verifier = "".join(secrets.choice(ALPHABET) for _ in range(128))
challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()

code_box = {}
class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        q = urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query)
        code_box.update({k: v[0] for k, v in q.items()})
        self.send_response(200); self.send_header("Content-Type", "text/html"); self.end_headers()
        self.wfile.write(b"<html><body>Login OK. Pode voltar ao terminal.</body></html>")
    def log_message(self, *a): pass

srv = http.server.HTTPServer(("127.0.0.1", 0), Handler)
port = srv.server_address[1]
redirect = f"http://127.0.0.1:{port}/"

authorize = (f"{AUTH}/auth/authorize?response_type=code&client_id={CLIENT_ID}"
             f"&redirect_uri={urllib.parse.quote(redirect, safe='')}"
             f"&scope=openid&code_challenge={challenge}&code_challenge_method=S256")
print(f"Abrindo o navegador para login. Se não abrir, acesse:\n{authorize}\n")
webbrowser.open(authorize)
threading.Thread(target=srv.handle_request).start()
# wait for the callback
import time
for _ in range(300):
    if "code" in code_box: break
    time.sleep(1)
srv.server_close()
if "code" not in code_box:
    print("Sem code no callback:", code_box); sys.exit(1)

def post_form(url, form, headers=None):
    data = urllib.parse.urlencode(form).encode()
    req = urllib.request.Request(url, data=data, headers=headers or {}, method="POST")
    req.add_header("Content-Type", "application/x-www-form-urlencoded")
    return urllib.request.urlopen(req, timeout=30)

tok = json.load(post_form(f"{AUTH}/auth/token", {
    "grant_type": "authorization_code", "client_id": CLIENT_ID,
    "redirect_uri": redirect, "code": code_box["code"],
    "code_verifier": verifier, "scope": "openid"}))
access = tok["access_token"]
print("token OK. expires_in=", tok.get("expires_in"), "scope=", tok.get("scope"))

# session token
login_req = urllib.request.Request(f"{API}v1/auth/login", data=b"", method="POST",
    headers={"Authorization": access, "X-SIMPLAY-APP-VERSION": APP_VERSION,
             "Content-Type": "application/json"})
login_resp = urllib.request.urlopen(login_req, timeout=30)
session_token = login_resp.headers.get("x-simplay-session-token", "")
print("session login:", login_resp.status, "session-token:", "yes" if session_token else "no")

# device list
dev_req = urllib.request.Request(f"{API}v1/video-devices?take=50&page=1",
    headers={"Authorization": access, "X-SIMPLAY-APP-VERSION": APP_VERSION,
             "X-SIMPLAY-SESSION-TOKEN": session_token})
devices = json.load(urllib.request.urlopen(dev_req, timeout=30))
with open(RAW_OUT, "w") as f:
    json.dump(devices, f, indent=2, ensure_ascii=False)

# redacted summary
def walk(o):
    if isinstance(o, dict):
        return {k: ("<REDACTED len=%d>" % len(v) if k == "password" and isinstance(v, str) else walk(v))
                for k, v in o.items()}
    if isinstance(o, list):
        return [walk(x) for x in o]
    return o
print(f"\nvideo-devices (bruto salvo em {RAW_OUT}):")
print(json.dumps(walk(devices), indent=2, ensure_ascii=False)[:3000])
