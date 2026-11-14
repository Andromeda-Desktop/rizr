import json
from http.server import BaseHTTPRequestHandler, HTTPServer

from app import list_items


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps(list_items()).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)


HTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
