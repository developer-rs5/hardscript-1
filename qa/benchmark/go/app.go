// HardScript benchmark app — identical API in Go (net/http).
// GET /                        -> {"ok":true}
// GET /hello/:name             -> {"hello":name}
// POST /echo                   -> {"echo":<json body>}
package main

import (
	"encoding/json"
	"io"
	"log"
	"net/http"
	"strings"
)

func writeJSON(w http.ResponseWriter, v any) {
	w.Header().Set("Content-Type", "application/json")
	_ = json.NewEncoder(w).Encode(v)
}

func root(w http.ResponseWriter, r *http.Request) { writeJSON(w, map[string]any{"ok": true}) }

func hello(w http.ResponseWriter, r *http.Request) {
	name := strings.TrimPrefix(r.URL.Path, "/hello/")
	writeJSON(w, map[string]any{"hello": name})
}

func echo(w http.ResponseWriter, r *http.Request) {
	body, err := io.ReadAll(r.Body)
	if err != nil {
		w.WriteHeader(400)
		return
	}
	var v any
	if err := json.Unmarshal(body, &v); err != nil {
		w.WriteHeader(400)
		return
	}
	writeJSON(w, map[string]any{"echo": v})
}

func main() {
	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/" {
			http.NotFound(w, r)
			return
		}
		root(w, r)
	})
	http.HandleFunc("/hello/", hello)
	http.HandleFunc("/echo", echo)
	log.Println("ready")
	log.Fatal(http.ListenAndServe("127.0.0.1:8080", nil))
}