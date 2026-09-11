package com.tryanks.tcode;

import android.text.Editable;
import android.text.Selection;
import android.view.View;
import android.view.inputmethod.BaseInputConnection;
import java.util.function.Consumer;

/** Publish Android's completed edits, including selection and composing-span changes. */
class GpuiInputConnection extends BaseInputConnection {
    record State(String text, int selectionStart, int selectionEnd, int composingStart, int composingEnd) {}
    private final Editable editable;
    private final Consumer<State> changed;
    private int batchDepth;
    private boolean closed;
    private State lastState;

    GpuiInputConnection(View view, Editable editable, Consumer<State> changed) {
        super(view, true);
        this.editable = editable;
        this.changed = changed;
        rememberState();
    }

    @Override public Editable getEditable() { return closed ? null : editable; }

    @Override public void closeConnection() {
        // restartInput retires the old connection after the app has supplied its new state.
        // BaseInputConnection.closeConnection would clear that new composing region.
        closed = true;
    }

    @Override public boolean beginBatchEdit() {
        batchDepth++;
        return true;
    }

    @Override public boolean endBatchEdit() {
        if (batchDepth == 0) return false;
        batchDepth--;
        publishChanges();
        return batchDepth > 0;
    }

    @Override public boolean setSelection(int start, int end) {
        boolean result = super.setSelection(start, end);
        publishChanges();
        return result;
    }

    void publishChanges() {
        if (closed || batchDepth != 0) return;
        State state = readState();
        if (!state.equals(lastState)) {
            lastState = state;
            changed.accept(state);
        }
    }

    void rememberState() { lastState = readState(); }

    private State readState() {
        return new State(editable.toString(), Selection.getSelectionStart(editable),
                Selection.getSelectionEnd(editable), getComposingSpanStart(editable),
                getComposingSpanEnd(editable));
    }
}
