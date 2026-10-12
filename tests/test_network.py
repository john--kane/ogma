"""Permissioned guest HTTP networking against loopback fixtures only."""
import http.server
import json
import os
from pathlib import Path
import subprocess
import threading
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
CLI = ROOT / 'artifacts' / 'ogma'
LIMIT = 16 * 1024 * 1024


class HttpFixture:
    def __init__(self):
        fixture = self
        self.requests = []
        self.other = None

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.1'

            def do_GET(self):
                fixture.requests.append(self.path)
                try:
                    if self.path in ['/redirect', '/host', '/port', '/loop']:
                        destinations = {'/redirect': '/text',
                                        '/host': 'http://localhost:' + str(fixture.port) + '/text',
                                        '/port': fixture.other + '/text' if fixture.other else '/text',
                                        '/loop': '/loop'}
                        self.send_response(302)
                        self.send_header('Location', destinations[self.path])
                        self.send_header('Content-Length', '0')
                        self.end_headers()
                    elif self.path == '/oversized-length':
                        self.send_response(200)
                        self.send_header('Content-Length', str(LIMIT + 1))
                        self.end_headers()
                        self.wfile.write(b'x')
                    elif self.path == '/oversized-chunks':
                        self.send_response(200)
                        self.send_header('Transfer-Encoding', 'chunked')
                        self.end_headers()
                        block = b'x' * (512 * 1024)
                        for _ in range(34):
                            self.wfile.write(format(len(block), 'x').encode() + b'\r\n')
                            self.wfile.write(block + b'\r\n')
                            self.wfile.flush()
                        self.wfile.write(b'0\r\n\r\n')
                    else:
                        if self.path == '/slow':
                            time.sleep(3)
                        body = b'\xff' if self.path == '/invalid-utf8' else 'héllo\0 😀'.encode()
                        self.send_response(404 if self.path == '/missing' else 200)
                        self.send_header('Content-Length', str(len(body)))
                        self.end_headers()
                        self.wfile.write(body)
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def log_message(self, *_):
                pass

        self.server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_port
        self.url = 'http://127.0.0.1:' + str(self.port)
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=0.05), daemon=True)
        self.thread.start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()


class Network(unittest.TestCase):
    def setUp(self):
        self.assertTrue(CLI.exists(), 'Build NativeAOT CLI first')
        self.http = HttpFixture()
        self.addCleanup(self.http.close)
        self.grant = '--allow-net=127.0.0.1:' + str(self.http.port)

    def fetch(self, path='/text', *flags, url=None, environment=None):
        url = url or self.http.url + path
        source = 'const text = await Ogma.fetchText(' + json.dumps(url) + '); console.log(text);'
        result = subprocess.run([str(CLI), 'eval', source, '--json', *flags],
                                capture_output=True, text=True, cwd=ROOT, env=environment, timeout=10)
        self.assertTrue(result.stdout, result.stderr)
        return result, json.loads(result.stdout)

    def error(self, result, value, code):
        self.assertNotEqual(result.returncode, 0, value)
        self.assertEqual(value['diagnostics'][0]['code'], code, value)

    def test_deny_by_default_does_not_open_a_socket(self):
        result, value = self.fetch()
        self.error(result, value, 'OGMA_PERMISSION_DENIED')
        self.assertEqual(self.http.requests, [])

    def test_exact_grant_fetches_utf8_and_embedded_nul(self):
        result, value = self.fetch('/text', self.grant)
        self.assertEqual(result.returncode, 0, value)
        self.assertEqual(value['output'][0]['text'], 'héllo\0 😀')
        self.assertEqual(self.http.requests, ['/text'])

    def test_redirects_recheck_hosts_and_ports_before_request(self):
        other = HttpFixture()
        self.addCleanup(other.close)
        self.http.other = other.url
        for path in ['/host', '/port']:
            with self.subTest(path=path):
                self.http.requests.clear()
                result, value = self.fetch(path, self.grant)
                self.error(result, value, 'OGMA_PERMISSION_DENIED')
                self.assertEqual(self.http.requests, [path])
                self.assertEqual(other.requests, [])
        result, value = self.fetch('/port', self.grant, '--allow-net=127.0.0.1:' + str(other.port))
        self.assertEqual(result.returncode, 0, value)
        self.assertEqual(other.requests, ['/text'])

    def test_same_origin_redirect_and_finite_redirect_limit(self):
        result, value = self.fetch('/redirect', self.grant)
        self.assertEqual(result.returncode, 0, value)
        self.assertEqual(self.http.requests, ['/redirect', '/text'])
        self.http.requests.clear()
        result, value = self.fetch('/loop', self.grant)
        self.error(result, value, 'OGMA_IO')
        self.assertEqual(len(self.http.requests), 6)

    def test_oversized_fixed_and_chunked_responses_are_bounded(self):
        for path in ['/oversized-length', '/oversized-chunks']:
            with self.subTest(path=path):
                result, value = self.fetch(path, self.grant)
                self.error(result, value, 'OGMA_RESOURCE_LIMIT')

    def test_http_failure_and_invalid_utf8_have_io_diagnostics(self):
        for path in ['/missing', '/invalid-utf8']:
            with self.subTest(path=path):
                result, value = self.fetch(path, self.grant)
                self.error(result, value, 'OGMA_IO')

    def test_userinfo_controls_and_non_http_urls_are_rejected(self):
        for url, code in [(self.http.url.replace('http://', 'http://user@'), 'OGMA_INVALID_ARGUMENT'),
                          (self.http.url.replace('http://', 'http:\\\\'), 'OGMA_INVALID_ARGUMENT'),
                          (self.http.url + '/\0', 'OGMA_INVALID_ARGUMENT'),
                          ('file:///etc/passwd', 'OGMA_UNSUPPORTED')]:
            result, value = self.fetch('/text', self.grant, url=url)
            self.error(result, value, code)
        self.assertEqual(self.http.requests, [])

    def test_environment_proxy_does_not_change_request_route(self):
        proxy = HttpFixture()
        self.addCleanup(proxy.close)
        environment = dict(os.environ, HTTP_PROXY=proxy.url, http_proxy=proxy.url,
                           ALL_PROXY=proxy.url, all_proxy=proxy.url, NO_PROXY='', no_proxy='')
        result, value = self.fetch('/text', self.grant, '--allow-env=HTTP_PROXY',
                                  '--allow-env=http_proxy', '--allow-env=ALL_PROXY',
                                  '--allow-env=all_proxy', '--allow-env=NO_PROXY',
                                  '--allow-env=no_proxy', environment=environment)
        self.assertEqual(result.returncode, 0, value)
        self.assertEqual(proxy.requests, [])
        self.assertEqual(self.http.requests, ['/text'])

    def test_supervisor_timeout_cancels_pending_http(self):
        started = time.monotonic()
        result, value = self.fetch('/slow', self.grant, '--timeout-ms=300')
        self.error(result, value, 'OGMA_TIMEOUT')
        self.assertLess(time.monotonic() - started, 2)


if __name__ == '__main__':
    unittest.main()
