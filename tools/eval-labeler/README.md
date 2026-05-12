# eval-labeler

Quick TK-based labeler for grading per-prompt precision against a running
engine.

```bash
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
python labeler.py --engine http://127.0.0.1:8089 --camera 1 --out labels.csv
```

Keys: **T** = true positive · **F** = false positive · **S** = skip · **Q** = quit.

When you quit, the script writes `labels.csv` and prints per-prompt precision:

```
label                     precision  tp / (tp+fp)
person                       92.50%    37 / 40
package                      40.00%     2 / 5
vehicle                      75.00%     3 / 4
```
