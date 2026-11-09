package denosanbox

import (
	"context"
	"encoding/json"
	"errors"
	"os"
	"reflect"
	"testing"
	"time"
)

func TestSharedBehaviorFixtures(t *testing.T) {
	data, err := os.ReadFile("../../fixtures/behavior/v1.json")
	if err != nil {
		t.Fatal(err)
	}
	var fixtures []struct {
		ID     string `json:"id"`
		Source string `json:"source"`
		Steps  []struct {
			Sandbox  int             `json:"sandbox"`
			Export   string          `json:"export"`
			Input    json.RawMessage `json:"input"`
			Expected struct {
				Kind  string `json:"kind"`
				Value any    `json:"value"`
				Code  string `json:"code"`
			} `json:"expected"`
		} `json:"steps"`
	}
	if err := json.Unmarshal(data, &fixtures); err != nil {
		t.Fatal(err)
	}
	for _, fixture := range fixtures {
		t.Run(fixture.ID, func(t *testing.T) {
			engine, err := New()
			if err != nil {
				t.Fatal(err)
			}
			defer func() {
				report, err := engine.Close()
				if err != nil || !report.Clean {
					t.Errorf("shutdown: %+v %v", report, err)
				}
			}()
			sandboxes := make([]*Sandbox, 2)
			for i := range sandboxes {
				sandboxes[i], err = engine.Create(fixture.Source)
				if err != nil {
					t.Fatal(err)
				}
				defer func(s *Sandbox) {
					_, err := s.Close()
					if err != nil {
						t.Error(err)
					}
				}(sandboxes[i])
			}
			for _, step := range fixture.Steps {
				output, err := sandboxes[step.Sandbox].Call(context.Background(), step.Export, step.Input, time.Second)
				if step.Expected.Kind == "err" {
					var native *Error
					if !errors.As(err, &native) || step.Expected.Code != "execution" || native.Status != StatusExecution {
						t.Fatalf("expected %s, got %v", step.Expected.Code, err)
					}
					continue
				}
				if err != nil {
					t.Fatal(err)
				}
				var value any
				if err := json.Unmarshal(output, &value); err != nil {
					t.Fatal(err)
				}
				if !reflect.DeepEqual(value, step.Expected.Value) {
					t.Fatalf("result %#v != %#v", value, step.Expected.Value)
				}
			}
		})
	}
}
