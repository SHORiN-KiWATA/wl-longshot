#!/usr/bin/env python3
import sys
import os
import argparse
import cv2
import numpy as np

def err(msg):
    sys.stderr.write(f"error: {msg}\n")
    sys.exit(1)

def ensure_bgra(img):
    if img.ndim == 2:
        return cv2.cvtColor(img, cv2.COLOR_GRAY2BGRA)
    if img.shape[2] == 4:
        return img
    return cv2.cvtColor(img, cv2.COLOR_BGR2BGRA)

def to_gray(img):
    if img.ndim == 2:
        return img
    if img.shape[2] == 4:
        return cv2.cvtColor(img, cv2.COLOR_BGRA2GRAY)
    return cv2.cvtColor(img, cv2.COLOR_BGR2GRAY)

def pad_width(img, width):
    h, w = img.shape[:2]
    if w >= width:
        return img

    pad_shape = (h, width - w, img.shape[2]) if img.ndim == 3 else (h, width - w)
    pad = np.zeros(pad_shape, dtype=img.dtype)
    return np.hstack((img, pad))

def stitch_incremental(base_path, next_path, out_path, min_overlap=20, threshold=15.0, dedup=True, keep_widths=False):
    img_base = cv2.imread(base_path, cv2.IMREAD_UNCHANGED)
    img_next = cv2.imread(next_path, cv2.IMREAD_UNCHANGED)
    if img_base is None or img_next is None:
        err("failed to read input images")

    h_base, w_base = img_base.shape[:2]
    h_next, w_next = img_next.shape[:2]

    if w_base != w_next and not keep_widths:
        img_next = cv2.resize(img_next, (w_base, h_next))
        h_next = img_next.shape[0]
        w_next = img_next.shape[1]

    if w_base != w_next and keep_widths:
        img_base = ensure_bgra(img_base)
        img_next = ensure_bgra(img_next)

    out_w = max(w_base, w_next)

    if not dedup:
        cv2.imwrite(out_path, np.vstack((pad_width(img_base, out_w), pad_width(img_next, out_w))))
        return

    search_h = min(h_base, h_next)
    search_w = min(w_base, w_next)
    img_base_tail = img_base[-search_h:]

    gray_base = to_gray(img_base_tail[:, :search_w])
    gray_next = to_gray(img_next[:, :search_w])
    edges_base = cv2.Canny(gray_base, 50, 150)
    edges_next = cv2.Canny(gray_next, 50, 150)

    best_overlap = 0
    min_diff = float('inf')

    for k in range(min_overlap, search_h):
        part1 = edges_base[search_h - k : search_h, :]
        part2 = edges_next[0 : k, :]
        
        if np.count_nonzero(part1) > 20 and np.count_nonzero(part2) > 20:
            diff = np.mean(np.abs(part1.astype(float) - part2.astype(float)))
            if diff < threshold and diff < min_diff:
                min_diff = diff
                best_overlap = k

    if best_overlap > 0:
        result = np.vstack((pad_width(img_base, out_w), pad_width(img_next[best_overlap:, :], out_w)))
    else:
        result = np.vstack((pad_width(img_base, out_w), pad_width(img_next, out_w)))

    cv2.imwrite(out_path, result)

def stitch_video(video_path, out_path):
    if not os.path.exists(video_path):
        err(f"video file not found: {video_path}")

    cap = cv2.VideoCapture(video_path)
    if not cap.isOpened():
        err("failed to open video")

    frames = []
    ret, prev_frame = cap.read()
    if not ret:
        err("video is empty")

    frames.append(prev_frame)
    anchor_frame = prev_frame.copy()

    h, w, _ = anchor_frame.shape
    x1, x2 = int(w * 0.15), int(w * 0.85)
    y1 = int(h * 0.15)
    template_h = int(h * 0.2)

    accumulated_shift = 0
    candidate_frame = None
    static_count = 0

    def get_grad(img):
        gray = cv2.cvtColor(img, cv2.COLOR_BGR2GRAY)
        return cv2.Sobel(gray, cv2.CV_8U, 0, 1, ksize=3)

    anchor_grad = get_grad(anchor_frame)

    while True:
        ret, curr_frame = cap.read()
        if not ret: break

        curr_grad = get_grad(curr_frame)
        template = curr_grad[y1 : y1 + template_h, x1:x2]
        roi = anchor_grad[y1:, x1:x2]

        res = cv2.matchTemplate(roi, template, cv2.TM_CCOEFF_NORMED)
        _, max_val, _, max_loc = cv2.minMaxLoc(res)
        shift = max_loc[1]

        if max_val > 0.5 and 2 < shift < (roi.shape[0] - 5):
            accumulated_shift += shift
            anchor_frame = curr_frame.copy()
            anchor_grad = curr_grad
            candidate_frame = curr_frame.copy()
            static_count = 0

            if accumulated_shift > h * 0.6:
                frames.append(candidate_frame[h - accumulated_shift:, :, :])
                accumulated_shift = 0
                candidate_frame = None
        else:
            static_count += 1
            if static_count > 10 and accumulated_shift > 0 and candidate_frame is not None:
                frames.append(candidate_frame[h - accumulated_shift:, :, :])
                accumulated_shift = 0
                candidate_frame = None
                anchor_frame = curr_frame.copy()
                anchor_grad = curr_grad

    cap.release()
    if accumulated_shift > 0 and candidate_frame is not None:
        frames.append(candidate_frame[h - accumulated_shift:, :, :])

    if len(frames) > 1:
        cv2.imwrite(out_path, np.vstack(frames), [cv2.IMWRITE_PNG_COMPRESSION, 3])
    else:
        cv2.imwrite(out_path, frames[0])

if __name__ == "__main__":
    parser = argparse.ArgumentParser(description="Core stitching engine for wl-longshot")
    subparsers = parser.add_subparsers(dest="mode", required=True)

    parser_inc = subparsers.add_parser("incremental")
    parser_inc.add_argument("--base", required=True)
    parser_inc.add_argument("--next", required=True)
    parser_inc.add_argument("--out", required=True)
    parser_inc.add_argument("--no-dedup", action="store_true")
    parser_inc.add_argument("--keep-widths", action="store_true")

    parser_vid = subparsers.add_parser("video")
    parser_vid.add_argument("--in", dest="input", required=True)
    parser_vid.add_argument("--out", required=True)

    args = parser.parse_args()

    if args.mode == "incremental":
        stitch_incremental(args.base, args.next, args.out, dedup=not args.no_dedup, keep_widths=args.keep_widths)
    elif args.mode == "video":
        stitch_video(args.input, args.out)
