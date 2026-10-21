package main

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"tailscale.com/client/tailscale/apitype"
	"tailscale.com/tailcfg"
)

const testCap = tailcfg.PeerCapability("timcki.xyz/cap/edit-proposed-text")

// peers maps a remote address to the node behind it. nodes named "granted-*"
// hold the capability.
type peers map[string]string

func (p peers) WhoIs(_ context.Context, addr string) (*apitype.WhoIsResponse, error) {
	name, ok := p[addr]
	if !ok {
		return nil, errors.New("no such peer")
	}
	who := &apitype.WhoIsResponse{Node: &tailcfg.Node{Name: name, StableID: tailcfg.StableNodeID("n-" + name)}}
	if strings.HasPrefix(name, "granted-") {
		who.CapMap = tailcfg.PeerCapMap{testCap: nil}
	}
	return who, nil
}

type fakeEditor struct {
	opened []string
	exits  map[string]chan int
}

func (f *fakeEditor) launch(path string) (func() int, error) {
	f.opened = append(f.opened, path)
	ch := make(chan int, 1)
	f.exits[path] = ch
	return func() int { return <-ch }, nil
}

func newTestStore(t *testing.T) (*editStore, *fakeEditor) {
	t.Helper()
	editor := &fakeEditor{exits: map[string]chan int{}}
	return &editStore{
		dir: t.TempDir(),
		whois: peers{
			"100.64.0.1:1000": "granted-bigchungus",
			"100.64.0.2:1000": "granted-other",
			"100.64.0.3:1000": "phone",
		},
		cap:    testCap,
		launch: editor.launch,
	}, editor
}

func call(t *testing.T, s *editStore, from, method, path, body string) (int, map[string]any) {
	t.Helper()
	req := httptest.NewRequest(method, path, strings.NewReader(body))
	req.RemoteAddr = from
	rec := httptest.NewRecorder()
	s.routes().ServeHTTP(rec, req)
	var out map[string]any
	if err := json.Unmarshal(rec.Body.Bytes(), &out); err != nil {
		t.Fatalf("%s %s: invalid json %q", method, path, rec.Body.String())
	}
	return rec.Code, out
}

// close simulates closing the editor tab and waits for the exit to be recorded.
func closeEditor(t *testing.T, s *editStore, editor *fakeEditor, id, path string, code int) {
	t.Helper()
	editor.exits[path] <- code
	done := filepath.Join(s.dir, id, "editor.done")
	deadline := time.Now().Add(5 * time.Second)
	for !exists(done) {
		if time.Now().After(deadline) {
			t.Fatalf("editor exit for %s was never recorded", id)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func TestEditRoundTrip(t *testing.T) {
	s, editor := newTestStore(t)
	const me = "100.64.0.1:1000"

	code, out := call(t, s, me, "POST", "/v1/edits", `{"text":"before\n","extension":"md","purpose":"  Linear comment\nINT-3919 "}`)
	if code != http.StatusCreated || out["status"] != "pending" {
		t.Fatalf("create: %d %v", code, out)
	}
	id := out["edit_id"].(string)
	if len(editor.opened) != 1 || filepath.Base(editor.opened[0]) != "linear-comment-int-3919.md" {
		t.Fatalf("opened %v", editor.opened)
	}
	path := editor.opened[0]

	// saving alone must not complete an open editor
	os.WriteFile(path, []byte(" after\n\nżółć\n"), 0o600)
	if _, out := call(t, s, me, "GET", "/v1/edits/"+id, ""); out["status"] != "pending" {
		t.Fatalf("before close: %v", out)
	}

	closeEditor(t, s, editor, id, path, 0)
	for range 2 {
		_, out = call(t, s, me, "GET", "/v1/edits/"+id, "")
		if out["status"] != "completed" || out["text"] != " after\n\nżółć\n" || out["changed"] != true {
			t.Fatalf("completed: %v", out)
		}
	}
}

func TestUnchangedAndFailedEdits(t *testing.T) {
	s, editor := newTestStore(t)
	const me = "100.64.0.1:1000"

	_, out := call(t, s, me, "POST", "/v1/edits", `{"text":"same"}`)
	same := out["edit_id"].(string)
	if filepath.Ext(editor.opened[0]) != ".md" || filepath.Base(editor.opened[0]) != "proposed-text.md" {
		t.Fatalf("defaults: %v", editor.opened)
	}
	closeEditor(t, s, editor, same, editor.opened[0], 0)
	if _, out := call(t, s, me, "GET", "/v1/edits/"+same, ""); out["changed"] != false {
		t.Fatalf("unchanged: %v", out)
	}

	_, out = call(t, s, me, "POST", "/v1/edits", `{"text":"x","extension":"gitcommit","purpose":"commit message"}`)
	failed := out["edit_id"].(string)
	closeEditor(t, s, editor, failed, editor.opened[1], 19)
	if _, out := call(t, s, me, "GET", "/v1/edits/"+failed, ""); out["status"] != "failed" || out["exit_code"] != float64(19) {
		t.Fatalf("failed: %v", out)
	}
}

func TestCancellationOverridesLaterCompletion(t *testing.T) {
	s, editor := newTestStore(t)
	const me = "100.64.0.1:1000"

	_, out := call(t, s, me, "POST", "/v1/edits", `{"text":"before"}`)
	id := out["edit_id"].(string)
	for range 2 {
		if code, out := call(t, s, me, "DELETE", "/v1/edits/"+id, ""); code != http.StatusOK || out["status"] != "canceled" {
			t.Fatalf("cancel: %d %v", code, out)
		}
	}
	os.WriteFile(editor.opened[0], []byte("must not be accepted"), 0o600)
	closeEditor(t, s, editor, id, editor.opened[0], 0)
	if _, out := call(t, s, me, "GET", "/v1/edits/"+id, ""); out["status"] != "canceled" {
		t.Fatalf("after cancel: %v", out)
	}
}

func TestOnlyGrantedOwnersReachEdits(t *testing.T) {
	s, editor := newTestStore(t)

	for _, from := range []string{"100.64.0.3:1000", "100.64.0.9:1000"} {
		if code, _ := call(t, s, from, "POST", "/v1/edits", `{"text":"x"}`); code != http.StatusForbidden {
			t.Fatalf("%s create: %d", from, code)
		}
		if code, _ := call(t, s, from, "GET", "/v1/health", ""); code != http.StatusForbidden {
			t.Fatalf("%s health: %d", from, code)
		}
	}
	if len(editor.opened) != 0 {
		t.Fatalf("editor opened for a denied peer: %v", editor.opened)
	}

	_, out := call(t, s, "100.64.0.1:1000", "POST", "/v1/edits", `{"text":"mine"}`)
	id := out["edit_id"].(string)
	for _, method := range []string{"GET", "DELETE"} {
		if code, _ := call(t, s, "100.64.0.2:1000", method, "/v1/edits/"+id, ""); code != http.StatusNotFound {
			t.Fatalf("other node %s: %d", method, code)
		}
	}
	if code, out := call(t, s, "100.64.0.1:1000", "GET", "/v1/health", ""); code != http.StatusOK || out["caller"] != "granted-bigchungus" {
		t.Fatalf("health: %d %v", code, out)
	}
}

func TestRejectsBadInput(t *testing.T) {
	s, editor := newTestStore(t)
	const me = "100.64.0.1:1000"

	for _, body := range []string{
		`{"text":"x","extension":"../md"}`,
		`{"text":"x","extension":"m d"}`,
		`not json`,
		`{"text":"` + strings.Repeat("x", maxBodyBytes) + `"}`,
	} {
		if code, _ := call(t, s, me, "POST", "/v1/edits", body); code != http.StatusBadRequest {
			t.Fatalf("%.40s: %d", body, code)
		}
	}
	for _, id := range []string{"edit-123", "edit-" + strings.Repeat("A", 32), "edit-" + strings.Repeat("g", 32)} {
		if code, _ := call(t, s, me, "GET", "/v1/edits/"+id, ""); code != http.StatusNotFound {
			t.Fatalf("id %q: %d", id, code)
		}
	}
	for range maxPendingPerOwner {
		call(t, s, me, "POST", "/v1/edits", `{"text":"x"}`)
	}
	if code, _ := call(t, s, me, "POST", "/v1/edits", `{"text":"x"}`); code != http.StatusTooManyRequests {
		t.Fatalf("over limit: %d", code)
	}
	if len(editor.opened) != maxPendingPerOwner {
		t.Fatalf("opened %d editors", len(editor.opened))
	}
}

func TestRecoverFailsEditsFromAPreviousRun(t *testing.T) {
	s, _ := newTestStore(t)
	const me = "100.64.0.1:1000"
	_, out := call(t, s, me, "POST", "/v1/edits", `{"text":"x"}`)
	id := out["edit_id"].(string)

	restarted := &editStore{dir: s.dir, whois: s.whois, cap: s.cap, launch: s.launch}
	restarted.failInterrupted()
	if _, out := call(t, restarted, me, "GET", "/v1/edits/"+id, ""); out["status"] != "failed" || out["exit_code"] != float64(interruptedExitCode) {
		t.Fatalf("after restart: %v", out)
	}
}

func TestSlug(t *testing.T) {
	for in, want := range map[string]string{
		"proposed text":           "proposed-text",
		"PR description":          "pr-description",
		"Linear comment INT-3919": "linear-comment-int-3919",
		"żółć":                    "proposed-text",
		strings.Repeat("ab ", 40): strings.TrimRight(strings.Repeat("ab-", 20), "-"),
	} {
		if got := slug(in); got != want {
			t.Errorf("slug(%q) = %q, want %q", in, got, want)
		}
	}
}
