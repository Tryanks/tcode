package com.tryanks.tcode;

import android.test.AndroidTestCase;
import android.text.Selection;
import android.text.SpannableStringBuilder;
import android.view.View;
import java.util.ArrayList;

/** Exercises the production connection against Android's real Editable/InputConnection code. */
@SuppressWarnings("deprecation")
public final class GpuiInputConnectionTest extends AndroidTestCase {
    public void testAutocompletePublishesTheCompleteReplacementOnce() {
        for (boolean composing : new boolean[] {false, true}) {
            SpannableStringBuilder text = new SpannableStringBuilder("an exmple here");
            Selection.setSelection(text, 9);
            ArrayList<GpuiInputConnection.State> sent = new ArrayList<>();
            GpuiInputConnection input = new GpuiInputConnection(new View(getContext()), text, sent::add);
            input.beginBatchEdit();
            if (composing) {
                input.setComposingRegion(3, 9);
            } else {
                input.deleteSurroundingText(6, 0);
            }
            input.commitText("example", 1);
            assertTrue(sent.isEmpty());
            input.endBatchEdit();
            assertEquals(1, sent.size());
            assertEquals(new GpuiInputConnection.State("an example here", 10, 10, -1, -1), sent.get(0));
        }
    }

    public void testSelectionAndUnicodeDeletionReachTheComposer() {
        SpannableStringBuilder text = new SpannableStringBuilder("😀word中");
        Selection.setSelection(text, text.length());
        ArrayList<GpuiInputConnection.State> sent = new ArrayList<>();
        GpuiInputConnection input = new GpuiInputConnection(new View(getContext()), text, sent::add);
        input.setSelection(2, 6);
        assertEquals(new GpuiInputConnection.State("😀word中", 2, 6, -1, -1), sent.get(0));
        input.commitText("example", 0);
        assertEquals(new GpuiInputConnection.State("😀example中", 2, 2, -1, -1), sent.get(1));
        input.deleteSurroundingTextInCodePoints(1, 0);
        assertEquals(new GpuiInputConnection.State("example中", 0, 0, -1, -1), sent.get(2));
        input.deleteSurroundingText(0, 7);
        assertEquals(new GpuiInputConnection.State("中", 0, 0, -1, -1), sent.get(3));
    }

    public void testRetiredConnectionCannotChangeTheNextDraft() {
        SpannableStringBuilder text = new SpannableStringBuilder("old");
        Selection.setSelection(text, text.length());
        ArrayList<GpuiInputConnection.State> sent = new ArrayList<>();
        GpuiInputConnection input = new GpuiInputConnection(new View(getContext()), text, sent::add);
        input.closeConnection();
        text.replace(0, text.length(), "new");
        input.commitText("stale suggestion", 1);
        assertEquals("new", text.toString());
        assertTrue(sent.isEmpty());
    }
}
