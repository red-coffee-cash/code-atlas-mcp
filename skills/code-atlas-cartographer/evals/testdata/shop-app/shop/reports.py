import csv
import subprocess


def export_orders(orders, path):
    with open(path, "w", newline="") as f:
        writer = csv.writer(f)
        for row in orders.all_for_export():
            writer.writerow(row)
    subprocess.run("gzip -f " + path, shell=True, check=True)
    return path + ".gz"
